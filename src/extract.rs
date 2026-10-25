//! Optional local parsers, with bounded output and no uploaded code execution.
use crate::error::ApiError;
use serde::Deserialize;
use std::{path::Path, process::Stdio, time::Duration};
use tokio::{io::AsyncReadExt, process::Command, time::timeout};

const MAX_OUTPUT: u64 = 10 * 1024 * 1024;
#[derive(Debug, Deserialize)]
pub struct ExtractedPage {
    pub text: String,
    pub page: Option<i64>,
    /// Present only on knowledge documents; student attachment parsing does
    /// not create durable page derivatives.
    #[serde(default)]
    pub image_key: Option<String>,
}
pub struct ExtractedDocument {
    pub pages: Vec<ExtractedPage>,
    pub message: String,
}

fn python_command() -> Command {
    if let Ok(path) = std::env::var("KB_PYTHON_BIN") {
        return Command::new(path);
    }
    if cfg!(windows) {
        let mut command = Command::new("py");
        command.arg("-3");
        command
    } else {
        Command::new("python3")
    }
}

pub(crate) fn poppler_command(name: &str) -> Command {
    if let Some(binary) = std::env::var_os(format!("KB_{}_BIN", name.to_ascii_uppercase())) {
        return Command::new(binary);
    }
    if let Ok(dir) = std::env::var("KB_POPPLER_BIN_DIR") {
        let suffix = if cfg!(windows) { ".exe" } else { "" };
        return Command::new(Path::new(&dir).join(format!("{name}{suffix}")));
    }
    Command::new(name)
}

// PDF text is parsed by a pinned code dependency rather than an old Windows
// Xpdf binary that can emit invalid surrogate encodings for math glyphs.
// Text extraction still does NOT recover the original equation tree.
pub(crate) async fn extract_pdf(
    path: &Path,
    max_pages: usize,
) -> Result<Vec<ExtractedPage>, ApiError> {
    let mut command = python_command();
    command
        .args(["-I", "-c", include_str!("../processing/pdf.py")])
        .arg(path)
        .arg(max_pages.to_string());
    let output = run_bounded(&mut command, path).await?.ok_or_else(|| {
        ApiError::unavailable(
            "PDF 解析需要 Python 3 和 pypdf，请安装 processing/requirements.txt 中的依赖",
        )
    })?;
    parse_pdf_output(&output, max_pages)
}

fn parse_pdf_output(output: &[u8], max_pages: usize) -> Result<Vec<ExtractedPage>, ApiError> {
    #[derive(Deserialize)]
    struct Manifest {
        #[serde(default)]
        pages: Vec<ExtractedPage>,
        error: Option<String>,
    }
    let manifest: Manifest = serde_json::from_slice(output)
        .map_err(|_| ApiError::bad_request("PDF 正文解析结果无效，未生成可审核正文"))?;
    match manifest.error.as_deref() {
        Some("missing_dependency") => return Err(ApiError::unavailable(
            "PDF 解析缺少 pypdf，请用 KB_PYTHON_BIN 对应的 Python 安装 processing/requirements.txt 中的依赖",
        )),
        Some("content_limit") => return Err(ApiError::bad_request("PDF 展开内容或正文超过处理限制，请拆分后上传")),
        Some("page_limit") => return Err(ApiError::bad_request(format!("PDF 页数超过处理上限 {max_pages} 页，请拆分后上传"))),
        Some("invalid_text") => return Err(ApiError::bad_request("PDF 数学字符无法完整解码，请转换文件或补充校对正文")),
        Some(_) => return Err(ApiError::bad_request("PDF 无法安全解析，请确认未损坏或加密")),
        None => {}
    }
    if manifest.pages.is_empty()
        || manifest.pages.len() > max_pages
        || manifest
            .pages
            .iter()
            .enumerate()
            .any(|(index, page)| page.page != Some(index as i64 + 1) || page.image_key.is_some())
    {
        return Err(ApiError::bad_request(
            "PDF 正文页码无效，已停止导入以避免引用错页",
        ));
    }
    Ok(manifest.pages)
}

pub async fn extract_document(
    root: &std::path::Path,
    key: &str,
    mime: &str,
) -> Result<ExtractedDocument, ApiError> {
    if key.contains(['/', '\\']) || key.starts_with('.') {
        return Err(ApiError::bad_request("无效的资料存储键"));
    }
    let path = root.join(key);
    if mime == "application/pdf" {
        let pages = extract_pdf(&path, 500).await?;
        return Ok(ExtractedDocument {
            pages,
            message: "已分页提取正文；公式上下标、分式和单位须对照原页校对，扫描页不会自动 OCR"
                .into(),
        });
    }
    let mut command = python_command();
    command.args(["-I", "-c", OOXML_EXTRACTOR]).arg(&path).arg(
        if mime.contains("presentationml") {
            "pptx"
        } else {
            "docx"
        },
    );
    let output = match run_bounded(&mut command, &path).await? {
        Some(output) => output,
        None => {
            return Ok(ExtractedDocument {
                pages: vec![],
                message: "本机解析器未安装，请安装 Python 或补充教师确认正文".into(),
            })
        }
    };
    let pages = serde_json::from_slice::<Vec<ExtractedPage>>(&output)
        .map_err(|_| ApiError::bad_request("Office 文件结构无效，未生成可审核正文"))?;
    let message = if pages.is_empty() {
        "未提取到正文；扫描PDF需OCR，其他资料可补充教师确认正文"
    } else if mime.contains("wordprocessingml") {
        "已提取正文及 Word 原生公式；公式已保留上下标、分式、根号和多行结构并转为 LaTeX，请仍逐项核对公式、单位、图表和专业术语后审核"
    } else {
        "正文已提取，请逐页核对公式、单位、图表和专业术语后审核；图片与声音不视为已理解"
    };
    Ok(ExtractedDocument {
        pages,
        message: message.into(),
    })
}

pub(crate) async fn run_bounded(
    command: &mut Command,
    _source: &Path,
) -> Result<Option<Vec<u8>>, ApiError> {
    command
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    #[cfg(windows)]
    command.creation_flags(0x08000000);
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(ApiError::unavailable("文档解析器暂不可用，请重试")),
    };
    let result = timeout(Duration::from_secs(60), async {
        let stdout = child.stdout.take().ok_or_else(ApiError::internal)?;
        let mut output = Vec::new();
        stdout
            .take(MAX_OUTPUT + 1)
            .read_to_end(&mut output)
            .await
            .map_err(|_| ApiError::unavailable("文档提取读取失败"))?;
        if output.len() as u64 > MAX_OUTPUT {
            return Err(ApiError::bad_request("文档提取文本超过10MB，请拆分后上传"));
        }
        let status = child
            .wait()
            .await
            .map_err(|_| ApiError::unavailable("文档解析进程失败"))?;
        if !status.success() {
            return Err(ApiError::bad_request(
                "文档无法安全解析，可能已加密、损坏或超过展开限制；请转换为文本或补充说明",
            ));
        }
        Ok(output)
    })
    .await;
    match result {
        Ok(Ok(output)) => Ok(Some(output)),
        Ok(Err(error)) => {
            let _ = child.kill().await;
            Err(error)
        }
        Err(_) => {
            let _ = child.kill().await;
            Err(ApiError::unavailable("文档提取超时，请拆分文件或稍后重试"))
        }
    }
}

// Standard-library-only extraction. Never unpack the archive or execute
// macros/relationships. XML DTD/entity constructs and excessive expansion are
// rejected before parsing; the archive is read only in bounded members.
const OOXML_EXTRACTOR: &str = include_str!("../processing/ooxml.py");

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pdf_manifest_keeps_math_unicode_blank_pages_and_rejects_invalid_alignment() {
        let pages = parse_pdf_output(
            r#"{"pages":[{"page":1,"text":"中文 𝜎 = 1"},{"page":2,"text":""},{"page":3,"text":"最后一页"}]}"#.as_bytes(),
            3,
        ).unwrap();
        assert_eq!(pages[0].text, "中文 𝜎 = 1");
        assert_eq!(pages[1].text, "");
        assert_eq!(pages[2].page, Some(3));
        for invalid in [
            r#"{"pages":[{"page":2,"text":"错页"}]}"#,
            r#"{"pages":[{"page":1,"text":"\ud835"}]}"#,
            r#"{"pages":[{"page":1,"text":"a","image_key":"invented.jpg"}]}"#,
            r#"{"pages":[]}"#,
            r#"{"error":"invalid_text"}"#,
        ] {
            assert!(parse_pdf_output(invalid.as_bytes(), 3).is_err());
        }
        let error = parse_pdf_output(br#"{"error":"missing_dependency"}"#, 3).unwrap_err();
        assert_eq!(error.status, axum::http::StatusCode::SERVICE_UNAVAILABLE);
    }

    async fn fixture_and_extract(xml: &str) -> Result<Vec<ExtractedPage>, ApiError> {
        let path = std::env::temp_dir().join(format!("{}.docx", uuid::Uuid::new_v4()));
        let status=python_command().args(["-I","-c","import sys,zipfile; z=zipfile.ZipFile(sys.argv[1],'w'); z.writestr('word/document.xml',sys.argv[2]); z.close()"]).arg(&path).arg(xml).status().await.map_err(|_|ApiError::unavailable("test Python missing"))?;
        assert!(status.success());
        let mut command = python_command();
        command
            .args(["-I", "-c", OOXML_EXTRACTOR])
            .arg(&path)
            .arg("docx");
        let result = run_bounded(&mut command, &path).await;
        let _ = tokio::fs::remove_file(&path).await;
        let output = result?.ok_or_else(|| ApiError::unavailable("test Python missing"))?;
        serde_json::from_slice(&output).map_err(|_| ApiError::internal())
    }
    #[tokio::test]
    async fn extracts_paragraphs_and_rejects_entities() {
        let pages=fixture_and_extract(r#"<w:document xmlns:w="urn:word"><w:p><w:r><w:t>科学</w:t></w:r><w:r><w:t>课程</w:t></w:r></w:p><w:p><w:r><w:t>第二段</w:t></w:r></w:p></w:document>"#).await.unwrap();
        assert_eq!(pages.len(), 1);
        assert_eq!(pages[0].text, "科学课程\n第二段");
        assert_eq!(pages[0].page, None);
        assert!(
            fixture_and_extract(r#"<!DOCTYPE x [<!ENTITY a "private">]><x>&a;</x>"#)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn extracts_word_omml_math_as_latex_without_dropping_structure() {
        let pages = fixture_and_extract(
            r#"<w:document xmlns:w="urn:word" xmlns:m="urn:math"><w:body><w:p><w:r><w:t>截面 </w:t></w:r><m:oMath><m:sSup><m:e><m:r><m:t>10</m:t></m:r></m:e><m:sup><m:r><m:t>-24</m:t></m:r></m:sup></m:sSup><m:f><m:num><m:r><m:t>a</m:t></m:r></m:num><m:den><m:r><m:t>b</m:t></m:r></m:den></m:f></m:oMath></w:p></w:body></w:document>"#,
        )
        .await
        .unwrap();
        assert_eq!(pages.len(), 1);
        assert_eq!(pages[0].text, r"截面 \({10}^{-24}\frac{a}{b}\)");
    }
}
