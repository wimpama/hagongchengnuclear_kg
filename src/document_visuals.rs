//! Page-level text and visuals for PDF/PPTX knowledge documents.
//! Originals and accepted page images live in OSS; the renderer's workspace is
//! disposable and isolated. A failed/cancelled ingest removes every new image.
use std::{
    path::{Path, PathBuf},
    process::Stdio,
    sync::OnceLock,
    time::Duration,
};

use serde::Deserialize;
use tokio::{io::AsyncReadExt, process::Command, sync::Semaphore, time::timeout};
use uuid::Uuid;

use crate::{
    error::ApiError,
    extract::{extract_pdf, poppler_command, ExtractedPage},
    storage::BlobStore,
};

const MAX_OUTPUT: u64 = 10 * 1024 * 1024;
const MAX_JPEG: u64 = 6 * 1024 * 1024;
const MAX_PAGE_BYTES: usize = 100 * 1024 * 1024;
static RENDERER: OnceLock<Semaphore> = OnceLock::new();

pub struct PreparedDocument {
    pub pages: Vec<ExtractedPage>,
    pub message: String,
    keys: Vec<String>,
    storage: BlobStore,
    retained: bool,
}

impl PreparedDocument {
    pub fn retain(&mut self) {
        self.retained = true;
    }
}

impl Drop for PreparedDocument {
    fn drop(&mut self) {
        if self.retained || self.keys.is_empty() {
            return;
        }
        for key in &self.keys {
            let _ = std::fs::remove_file(self.storage.local_dir().join(key));
        }
        let keys = std::mem::take(&mut self.keys);
        let storage = self.storage.clone();
        tokio::spawn(async move {
            for key in keys {
                if storage.delete(&key).await.is_err() {
                    tracing::warn!(storage_key = %key, "failed to clean up uncommitted document page");
                }
            }
        });
    }
}

struct Workspace(PathBuf);
impl Workspace {
    async fn create(root: &Path) -> Result<Self, ApiError> {
        let path = root.join(format!("document-{}", Uuid::new_v4()));
        tokio::fs::create_dir_all(&path)
            .await
            .map_err(|_| ApiError::internal())?;
        Ok(Self(external_path(
            tokio::fs::canonicalize(&path)
                .await
                .map_err(|_| ApiError::internal())?,
        )))
    }
}
impl Drop for Workspace {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// PDF/PPTX never silently fall back to text-only extraction. DOCX retains
/// its existing bounded OOXML text parser.
pub async fn prepare(
    storage: &BlobStore,
    key: &str,
    mime: &str,
) -> Result<PreparedDocument, ApiError> {
    if key.contains(['/', '\\']) || key.starts_with('.') {
        return Err(ApiError::bad_request("无效的资料存储键"));
    }
    let mut document = PreparedDocument {
        pages: vec![],
        message: String::new(),
        keys: vec![],
        storage: storage.clone(),
        retained: false,
    };
    if mime != "application/pdf" && !mime.contains("presentationml") {
        let parsed = crate::extract::extract_document(storage.local_dir(), key, mime).await?;
        document.pages = parsed.pages;
        document.message = parsed.message;
        return Ok(document);
    }

    // One renderer across both site tenants bounds the small server's memory.
    let _permit = timeout(
        Duration::from_secs(15),
        RENDERER.get_or_init(|| Semaphore::new(1)).acquire(),
    )
    .await
    .map_err(|_| ApiError::unavailable("正在处理另一份图文资料，请稍后重试"))?
    .map_err(|_| ApiError::internal())?;
    let workspace = Workspace::create(storage.local_dir()).await?;
    let source = external_path(
        tokio::fs::canonicalize(storage.local_dir().join(key))
            .await
            .map_err(|_| ApiError::internal())?,
    );
    let result = timeout(Duration::from_secs(360), async {
        if mime == "application/pdf" {
            prepare_pdf(&source, &mut document).await
        } else {
            // Bound ZIP/XML expansion before the code-only slide renderer.
            let parsed = crate::extract::extract_document(storage.local_dir(), key, mime).await?;
            if parsed.message.contains("未安装") {
                return Err(ApiError::unavailable(
                    "PPTX 图文解析需要 Python 3，请配置 KB_PYTHON_BIN",
                ));
            }
            prepare_presentation(&source, &workspace.0, &mut document).await
        }
    })
    .await
    .map_err(|_| ApiError::unavailable("资料图文解析超时，请拆分为较小文件后重试"))?;
    result?;
    document.message = format!(
        "已提取 {} 页正文及页面图片，支持图文联合索引；请核对公式、单位、图表和专业术语后审核。扫描页保留原图，不会自动补写正文。",
        document.pages.len()
    );
    Ok(document)
}

fn max_pages() -> Result<usize, ApiError> {
    std::env::var("KB_DOCUMENT_MAX_PAGES")
        .unwrap_or_else(|_| "200".into())
        .parse::<usize>()
        .ok()
        .filter(|n| (1..=500).contains(n))
        .ok_or_else(|| ApiError::unavailable("KB_DOCUMENT_MAX_PAGES 必须为 1 到 500"))
}

fn page_count(info: &[u8], limit: usize) -> Result<usize, ApiError> {
    let count = std::str::from_utf8(info).ok().and_then(|s| {
        s.lines().find_map(|line| {
            line.strip_prefix("Pages:")
                .and_then(|v| v.trim().parse::<usize>().ok())
        })
    });
    match count {
        Some(n) if n > 0 && n <= limit => Ok(n),
        Some(n) if n > limit => Err(ApiError::bad_request(format!(
            "资料共 {n} 页，超过图文处理上限 {limit} 页，请拆分后上传"
        ))),
        _ => Err(ApiError::bad_request(
            "无法读取文档页数，请确认文件未损坏或加密",
        )),
    }
}

async fn prepare_pdf(pdf: &Path, document: &mut PreparedDocument) -> Result<(), ApiError> {
    let mut info = poppler_command("pdfinfo");
    info.env("LC_ALL", "C").arg(pdf);
    let info = run_required(&mut info, "pdfinfo", MAX_OUTPUT, 45).await?;
    let count = page_count(&info, max_pages()?)?;
    document.pages = extract_pdf(pdf, max_pages()?).await?;
    if document.pages.len() != count {
        return Err(ApiError::bad_request(
            "正文与页面数量不一致，已停止导入以避免引用错页",
        ));
    }
    let mut bytes_used = 0;
    for page in &mut document.pages {
        let number = page.page.ok_or_else(ApiError::internal)?.to_string();
        let mut render = poppler_command("pdftoppm");
        // Omitting an output prefix writes the single JPEG to stdout. Rendering
        // one page at a time bounds memory and never creates an unbounded bitmap.
        render
            .args([
                "-f",
                &number,
                "-l",
                &number,
                "-singlefile",
                "-scale-to",
                "1600",
                "-jpeg",
                "-jpegopt",
                "quality=88",
            ])
            .arg(pdf);
        let jpeg = run_required(&mut render, "pdftoppm", MAX_JPEG, 45).await?;
        if !jpeg.starts_with(&[0xff, 0xd8, 0xff]) {
            return Err(ApiError::bad_request("文档页面未生成有效图片，请重试"));
        }
        bytes_used += jpeg.len();
        if bytes_used > MAX_PAGE_BYTES {
            return Err(ApiError::bad_request(
                "页面图片合计超过 100MB，请拆分资料后上传",
            ));
        }
        let key = format!("{}.jpg", Uuid::new_v4());
        // Register before starting PUT so cancellation also cleans its object.
        document.keys.push(key.clone());
        document
            .storage
            .put_bytes(&key, &jpeg, "image/jpeg")
            .await?;
        page.image_key = Some(key);
    }
    Ok(())
}

// Some Windows Poppler builds do not understand Rust's canonical \\?\ paths.
// This changes only the path spelling; the filesystem target stays identical.
fn external_path(path: PathBuf) -> PathBuf {
    #[cfg(windows)]
    {
        let value = path.to_string_lossy();
        if let Some(rest) = value.strip_prefix(r"\\?\UNC\") {
            return PathBuf::from(format!(r"\\{rest}"));
        }
        if let Some(rest) = value.strip_prefix(r"\\?\") {
            return PathBuf::from(rest);
        }
    }
    path
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SlidePage {
    page: usize,
    text: String,
    image_file: String,
}
#[derive(Deserialize)]
struct SlideManifest {
    #[serde(default)]
    pages: Vec<SlidePage>,
    error: Option<String>,
}

async fn prepare_presentation(
    source: &Path,
    workspace: &Path,
    document: &mut PreparedDocument,
) -> Result<(), ApiError> {
    let processor = std::env::var_os("KB_PPTX_PROCESSOR")
        .map(PathBuf::from)
        .unwrap_or_else(|| Path::new(env!("CARGO_MANIFEST_DIR")).join("processing/pptx.mjs"));
    let mut command = Command::new(std::env::var("KB_NODE_BIN").unwrap_or_else(|_| "node".into()));
    command
        .arg("--max-old-space-size=384")
        .arg(processor)
        .arg(source)
        .arg(workspace)
        .arg(max_pages()?.to_string());
    let bytes = run_required(&mut command, "PPTX 代码解析器（Node.js）", MAX_OUTPUT, 240).await?;
    let manifest: SlideManifest = serde_json::from_slice(&bytes)
        .map_err(|_| ApiError::unavailable("PPTX 解析结果无效，请检查后端 processing 依赖"))?;
    if let Some(error) = manifest.error {
        return Err(ApiError::bad_request(
            error.chars().take(300).collect::<String>(),
        ));
    }
    if manifest.pages.is_empty() || manifest.pages.len() > max_pages()? {
        return Err(ApiError::bad_request("PPTX 解析页数无效，请拆分资料后重试"));
    }
    let mut bytes_used = 0;
    for (i, page) in manifest.pages.into_iter().enumerate() {
        if page.page != i + 1 || page.image_file != format!("{}.png", i + 1) {
            return Err(ApiError::bad_request(
                "PPTX 正文与图片页码不一致，已停止导入",
            ));
        }
        let path = workspace.join(page.image_file);
        let size = tokio::fs::metadata(&path)
            .await
            .map_err(|_| ApiError::internal())?
            .len();
        if size > MAX_OUTPUT {
            return Err(ApiError::bad_request("幻灯片图片过大，请拆分后上传"));
        }
        let image = tokio::fs::read(&path)
            .await
            .map_err(|_| ApiError::internal())?;
        bytes_used += image.len();
        if !image.starts_with(b"\x89PNG\r\n\x1a\n") || bytes_used > MAX_PAGE_BYTES {
            return Err(ApiError::bad_request("幻灯片图片无效或合计超过 100MB"));
        }
        let key = format!("{}.png", Uuid::new_v4());
        document.keys.push(key.clone());
        document
            .storage
            .put_bytes(&key, &image, "image/png")
            .await?;
        document.pages.push(ExtractedPage {
            text: page.text,
            page: Some(page.page as i64),
            image_key: Some(key),
        });
        tokio::fs::remove_file(path)
            .await
            .map_err(|_| ApiError::internal())?;
    }
    Ok(())
}

pub(crate) fn page_image_mime(key: &str) -> &'static str {
    if key.ends_with(".png") {
        "image/png"
    } else {
        "image/jpeg"
    }
}

async fn run_required(
    command: &mut Command,
    tool: &str,
    max_output: u64,
    seconds: u64,
) -> Result<Vec<u8>, ApiError> {
    command
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    #[cfg(windows)]
    command.creation_flags(0x08000000); // CREATE_NO_WINDOW for renderer helpers.
    let mut child = command.spawn().map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            ApiError::unavailable(format!(
                "缺少 {tool}，PDF/PPTX 必须完成图文解析才能导入。请检查后端 Poppler 与 Node.js 依赖，或配置 KB_POPPLER_BIN_DIR、KB_NODE_BIN"
            ))
        } else {
            ApiError::unavailable(format!("无法启动 {tool}，请检查工具配置"))
        }
    })?;
    let result = timeout(Duration::from_secs(seconds), async {
        let stdout = child.stdout.take().ok_or_else(ApiError::internal)?;
        let mut output = Vec::new();
        stdout
            .take(max_output + 1)
            .read_to_end(&mut output)
            .await
            .map_err(|_| ApiError::unavailable("文档图文解析读取失败"))?;
        if output.len() as u64 > max_output {
            return Err(ApiError::bad_request("文档解析输出超过限制，请拆分后上传"));
        }
        let status = child
            .wait()
            .await
            .map_err(|_| ApiError::unavailable("文档渲染进程失败"))?;
        if !status.success() {
            return Err(ApiError::bad_request(format!(
                "{tool} 无法解析文档，请确认文件未损坏或加密"
            )));
        }
        Ok(output)
    })
    .await;
    match result {
        Ok(Ok(output)) => Ok(output),
        Ok(Err(error)) => {
            let _ = child.kill().await;
            Err(error)
        }
        Err(_) => {
            let _ = child.kill().await;
            Err(ApiError::unavailable(format!(
                "{tool} 处理超时，请拆分文档后重试"
            )))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn page_count_limits_are_checked_before_rendering() {
        assert!(page_count(b"Pages: 201\n", 200).is_err());
        assert!(page_count(b"Pages: 0\n", 200).is_err());
        assert_eq!(page_count(b"Title: test\nPages: 3\n", 200).unwrap(), 3);
    }

    #[tokio::test]
    async fn missing_renderer_fails_instead_of_accepting_text_only() {
        let mut command = Command::new("nonexistent-kb-renderer-for-test");
        let error = run_required(&mut command, "test-renderer", 100, 1)
            .await
            .unwrap_err();
        assert_eq!(error.status, axum::http::StatusCode::SERVICE_UNAVAILABLE);
        assert!(error.message.contains("test-renderer"));
    }

    /// A text page, a graphics-only page and a final text page. Keeping the
    /// fixture self-contained also exercises real Poppler page alignment.
    fn sample_pdf() -> Vec<u8> {
        let contents = [
            "BT /F1 20 Tf 55 720 Td (NEUTRON CROSS SECTION) Tj ET 0 0.3 0.8 rg 55 500 220 90 re f",
            "0.8 0.1 0.1 rg 150 330 240 180 re f",
            "BT /F1 20 Tf 55 720 Td (FINAL PAGE) Tj ET",
        ];
        let mut objects = vec![
            "<< /Type /Catalog /Pages 2 0 R >>".to_owned(),
            "<< /Type /Pages /Kids [4 0 R 6 0 R 8 0 R] /Count 3 >>".to_owned(),
            "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_owned(),
        ];
        for (i, content) in contents.iter().enumerate() {
            objects.push(format!("<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Resources << /Font << /F1 3 0 R >> >> /Contents {} 0 R >>", 5 + 2 * i));
            objects.push(format!(
                "<< /Length {} >>\nstream\n{}\nendstream",
                content.len(),
                content
            ));
        }
        let mut pdf = b"%PDF-1.4\n".to_vec();
        let mut offsets = vec![0];
        for (i, object) in objects.iter().enumerate() {
            offsets.push(pdf.len());
            pdf.extend_from_slice(format!("{} 0 obj\n{}\nendobj\n", i + 1, object).as_bytes());
        }
        let xref = pdf.len();
        pdf.extend_from_slice(
            format!("xref\n0 {}\n0000000000 65535 f \n", offsets.len()).as_bytes(),
        );
        for offset in offsets.iter().skip(1) {
            pdf.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());
        }
        pdf.extend_from_slice(
            format!(
                "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
                offsets.len()
            )
            .as_bytes(),
        );
        pdf
    }

    #[tokio::test]
    #[ignore = "requires Node.js, installed processing dependencies, Python and a Chinese font"]
    async fn real_code_pptx_pipeline_pairs_page_text_images_without_office() {
        let workspace = Workspace::create(&std::env::temp_dir()).await.unwrap();
        let storage = BlobStore::new(crate::config::StorageConfig::Local {
            local_dir: workspace.0.clone(),
        });
        let key = format!("{}.pptx", Uuid::new_v4());
        let mut fixture =
            Command::new(std::env::var("KB_NODE_BIN").unwrap_or_else(|_| "node".into()));
        fixture
            .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("processing/pptx-fixture.mjs"))
            .arg(workspace.0.join(&key));
        run_required(&mut fixture, "PPTX test fixture", MAX_OUTPUT, 30)
            .await
            .unwrap();
        let mut document = prepare(
            &storage,
            &key,
            "application/vnd.openxmlformats-officedocument.presentationml.presentation",
        )
        .await
        .unwrap();
        assert_eq!(document.pages.len(), 3);
        assert_eq!(document.pages[0].page, Some(1));
        assert!(document.pages[0].text.is_empty());
        assert!(document.pages[1].text.contains("核反应截面"));
        assert!(document.pages[2].text.contains("CHART PAGE"));
        for page in &document.pages {
            let key = page.image_key.as_ref().unwrap();
            assert!(key.ends_with(".png"));
            assert!(
                tokio::fs::metadata(workspace.0.join(key))
                    .await
                    .unwrap()
                    .len()
                    > 1000
            );
        }
        let images = document.keys.clone();
        document.retain();
        drop(document);
        assert!(images.iter().all(|key| workspace.0.join(key).exists()));
    }

    #[tokio::test]
    #[ignore = "requires pypdf and real Poppler pdfinfo and pdftoppm"]
    async fn real_poppler_keeps_graphics_only_pages_and_cleans_failed_ingest_derivatives() {
        let workspace = Workspace::create(&std::env::temp_dir()).await.unwrap();
        let storage = BlobStore::new(crate::config::StorageConfig::Local {
            local_dir: workspace.0.clone(),
        });
        let key = format!("{}.pdf", Uuid::new_v4());
        tokio::fs::write(workspace.0.join(&key), sample_pdf())
            .await
            .unwrap();
        let document = prepare(&storage, &key, "application/pdf").await.unwrap();
        assert_eq!(document.pages.len(), 3);
        assert!(document.pages[0].text.contains("NEUTRON CROSS SECTION"));
        assert!(document.pages[1].text.is_empty());
        assert!(document.pages[2].text.contains("FINAL PAGE"));
        let images: Vec<_> = document
            .pages
            .iter()
            .map(|p| workspace.0.join(p.image_key.as_ref().unwrap()))
            .collect();
        for image in &images {
            let bytes = tokio::fs::read(image).await.unwrap();
            assert!(bytes.starts_with(&[0xff, 0xd8, 0xff]));
            assert!(bytes.len() > 1000);
        }
        // A caller that never commits its metadata must not retain page blobs.
        drop(document);
        assert!(images.iter().all(|path| !path.exists()));
        assert!(workspace.0.join(key).exists());
    }
}
