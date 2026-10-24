# 课程知识库后端

本目录 `back_end/` 是独立 Rust 项目，与同级 `front_end/` 各自保留 Git。一个 Rust 进程、一套 Compose 和一个 Qdrant 同时服务两个前端站点；前端仍由现有 Nginx 托管，打包命令是 `npm run build:future`。

## 站点与存储

| 请求站点（Host / Origin） | 资料库 | OSS 桶 | 原有鉴权接口 |
| --- | --- | --- | --- |
| localhost、127.0.0.1、futuredev.artz.zone | dev SQLite 与向量集合 | futuredev | futuredev.artz.zone/api |
| future.artz.zone | prod SQLite 与向量集合 | futureprod | future.artz.zone/api |

后端按精确域名选择环境；Host 与 Origin 指向不同环境时拒绝请求，不接受前端传入任意桶名或环境标记。Nginx 必须保留 `Host $host`。两站点可以使用相同课程 ID，但资料、鉴权、会话和新建向量集合相互隔离。未知域名需要先在服务端配置。

本新增 Rust 服务只管理自己的两份 SQLite：localhost、127.0.0.1 与 futuredev 在本服务内使用同一个测试数据库连接池，future 始终使用独立正式库。资料上传记录、校对与发布记录、会话、消息、附件记录和模型配置均按这两份库隔离，不能用同一个用户或课程 ID 跨环境读取。本服务不连接或修改现有其他业务服务的数据库；本机与服务器之间不做 SQLite 文件共享或自动同步。

所有课程原件、视频片段和抽帧保存到私有 OSS 的 `knowledge/doc/`；学生会话附件保存到 `knowledge/user_upload/`。既有课件的 `courseware/` 不由本服务修改。签名课件 URL 具有有效期，不保存为知识库原件地址。

资源存储只支持 OSS；连接失败明确报错。磁盘只保存 SQLite、Qdrant、日志和可丢弃的处理缓存。解析文档、FFmpeg、构造模型图片输入时按需取回缓存，空闲超过 24 小时自动回收；媒体预览直接从 OSS 转发并支持 Range，不必先下载整个视频。客户端只获得经过授权的服务端预览入口，不获得 OSS 凭证。

学生附件从上传起保留 30 天，到期立即禁止访问，后台每分钟清理原件和提取正文，历史显示失效提示。消息或摘要中已经讨论的题意不会随着附件过期自动删除。学生附件仅属于其会话，不成为教师审核的课程证据。

历史对话可重命名或删除。删除会话事务性地删除消息、摘要和附件引用，同时保存 OSS 清理任务；网络失败后仍会重试。删除课程资料会立即撤销发布与媒体访问，并由持久任务清理该资料的 Qdrant 向量及未被其他资料共用的 OSS 文件。仅撤回资料会保留原件，便于重新发布。

## Windows 本机启动

```powershell
cd F:\code_base\web_code\3-dcourse-hub-weilaiketang\back_end
$env:CARGO_INCREMENTAL = '0'
cargo run --locked
```

普通开发终端中，`cargo run` 自动读取本目录 `.env`，启动并等待本机 Qdrant 就绪，再启动 Rust HTTP 服务。Ctrl+C 会同时关闭 Rust 和本次启动的 Qdrant。Qdrant 缺失、端口占用或启动失败会明确报错，不接管其他进程；不会自动切到其他数据库。

兼容入口 `sh start.sh` / `start-local.ps1` 也会自动启动两项服务，无需另外手动启动 Qdrant。已有最新二进制时可用 `sh start.sh --no-build`。同一时间只运行一个启动命令。

首次在新机器使用时，从 `env.example` 复制 `.env`，填写 OSS、Ark、DeepSeek 密钥和固定的 32 字节 base64 `ENCRYPTION_MASTER_KEY`。已有 SQLite 数据库必须保留原主密钥，不能重新生成。需要 Rust 1.94+、Python 3、FFmpeg / FFprobe、Poppler，以及 `data/tools/qdrant/qdrant.exe`；本工作区已具备。其他 Qdrant 可执行文件路径可用 `KB_QDRANT_BIN` 指定。必要时在 `.env` 指定 `KB_PYTHON_BIN`，并用同一个解释器安装 `processing/requirements.txt`（Windows：`py -3 -m pip install -r processing/requirements.txt`）。PDF 正文由锁定版本 pypdf 解析，页面渲染需要 `pdfinfo`、`pdftoppm`，可通过 `KB_POPPLER_BIN_DIR` 或各工具变量指定路径。Windows 构建关闭增量编译，规避本机工具链曾出现的增量编译错误。

PPTX 使用 Node.js 22.18+ 的代码库解析与绘图，不需要 Office、LibreOffice 或浏览器。本机首次在后端执行 `npm ci --prefix processing` 安装锁定依赖，启动前执行 `nvm use 22.19.0`，或用 `KB_NODE_BIN` 指定 Node 完整路径。后端 Docker 构建自动包含 Node、处理代码及中文字体，Linux 服务器无需另外安装桌面软件。

本机端口：Rust 8080；Qdrant 16333 / 16334。前端另开终端：

```powershell
cd ..\front_end
nvm use 22.19.0
npm run dev
```

打开 http://localhost:5173/ 。Vite 固定端口且不自动跳到 5174；端口占用时先确认已有前端是否仍在运行。

前端 `.env.development` 将 `VITE_KB_API_BASE_URL` 设为 `http://127.0.0.1:8080/kb-api/v1`，由浏览器直连本机 Rust，无需 Vite 知识库代理。Rust 处理已知且环境匹配的 Origin / CORS 预检，支持 Authorization、SSE 与媒体 Range。正式构建的 `.env.future` 使用 `https://future.artz.zone/kb-api/v1`；测试站点应设置对应的 `https://futuredev.artz.zone/kb-api/v1`。线上域名仍通过下面的 Nginx 片段连接本服务。

```text
data/local/
  knowledge.sqlite         开发环境的课程结构、提取文字、发布记录、会话等
  prod/knowledge.sqlite    独立生产记录库
  qdrant/storage/          dev/prod 向量集合
  cache/dev/、cache/prod/  可丢弃的处理缓存
  logs/                    本机日志
  uploads/                 迁移前原件备份，运行服务不使用此目录
```

`/healthz` 检查进程，`/readyz` 检查 SQLite 和 Qdrant。通过相应 Host 请求可分别检查两环境；这不替代账户和课程权限验证。

### 启动参数

普通开发终端中直接 `cargo run --locked` 即可。如果终端已预先设置 `KB_BIND_ADDR` 或 `QDRANT_HTTP_URL`，服务会尊重这套外部配置；要明确使用自动管理的本机开发模式：

```powershell
cargo run --locked -- --local-preview
```

本机模式读取 `.env`，使用已有 `data/local/knowledge.sqlite`、独立 prod 记录库及 `qdrant/storage` 向量索引；自动管理 16333 / 16334 的 Qdrant 子进程。这里保留的是数据库记录和向量索引，上传文件原件仍写入 OSS；文档解析、抽帧和模型输入使用可清理的临时缓存。自定义配置文件可用 `cargo run --locked -- --env-file .env --local-preview`。

Docker release 环境或已由脚本配置的进程使用预先配置的 Qdrant，由 Compose / 外层启动器统一管理。Rust 不会再在容器内额外启动一个 Qdrant。`cargo run -- --help` 可查看参数。

## Linux / Docker

服务器从 Git 更新、构建容器并启动的统一入口是 `sh deploy.sh`。首次 clone、运行配置、独立 Nginx 命令见 [部署说明](deploy/部署说明.md)。`start.sh` 仅启动当前目录代码，不负责 Git 更新。

只启动一套，无需按两个前端再部署两份后端。配置在运行时注入，不编译进镜像：

```bash
cd back_end
cp env.example .env
chmod 600 .env
# 填写凭证与固定主密钥；KB_HOST_DATA_DIR 设为稳定绝对路径
# 以下以新空目录 /srv/course-knowledge 为例，已有数据不得直接替换
sudo install -d -o 10001 -g 10001 /srv/course-knowledge
sudo install -d -o 10001 -g 10001 /srv/course-knowledge/dev /srv/course-knowledge/prod /srv/course-knowledge/cache
sudo install -d /srv/course-knowledge/qdrant
sh start.sh
```

`.env` 中设置 `KB_HOST_DATA_DIR=/srv/course-knowledge`。SQLite 默认位于该目录的 dev / prod 子目录。已有部署如果使用旧数据库路径，先备份并明确迁移或设置 `KB_DEV_DATABASE_URL` / `KB_PROD_DATABASE_URL` 为实际容器内路径；不能将新的空库当作旧库。启动时数据库会绑定环境，阻止同一数据库被两环境共用。

Rust API 只映射宿主机 `127.0.0.1:8080`，Qdrant 只在 Compose 私网可达。Rust / Qdrant 默认各 0.5 CPU、768 MiB；共享的 2 vCPU / 4 GiB 服务器还运行原业务，实际并发能力需另行压测。可在别的 Linux 构建机执行 `docker compose build`，用 `docker save/load` 传镜像，再用 `sh start.sh --no-build` 启动。

两个站点的 TLS server 块都加入 `deploy/nginx-kb.conf` 中的 `/kb-api/` 片段，均代理到 8080，保留 `proxy_set_header Host $host`，保留现有 `/api`、`/ws` 和前端静态目录。不要整体覆盖原 Nginx 配置。模型推理、审核和压缩可能连续调用，片段设置了长连接读取超时并关闭代理缓冲。

日常查看：`docker compose -p course-knowledge logs --tail 100 knowledge-service`；Git 更新部署：`sh deploy.sh`；停止：`docker compose -p course-knowledge stop`。保留数据目录、OSS 原件、Qdrant 快照和主密钥备份，不使用删除卷的命令。当前开发机没有 Docker，Linux 镜像与线上部署仍待实际发布时验证。

通过 Git 管理的服务器日常更新使用 `sh deploy.sh`，它会先拉取代码并构建，构建成功后替换容器；`start.sh` 适合启动已经准备好的本地源码 / 镜像。部署控制流程的离线回归可运行 `py -3 -m unittest discover -s tests -p test_deploy.py -v`；使用临时 Git 仓库和模拟 Docker，不连接服务器。

## 课程资料与已有课件导入

“同步课程结构”只更新原业务课程、一级和二级节点的镜像，用于确定资料归属与问答范围；不会导入课件、上传文件或调用向量模型。课程新增、改名、移动、删除后可同步，页面会显示同步数量。

知识库上传表单支持选择本地文件，或点击旁边的“从已有课件导入”，选择当前二级节点已有的课件；随后填写名称、可选说明和字幕并确认归属。后端只接受课件 ID，重新通过管理员接口核对归属和文件来源，不能导入其他节点课件或任意 URL。来源域名按 dev/prod 分别配置在 `KB_DEV_COURSEWARE_HOSTS` / `KB_PROD_COURSEWARE_HOSTS`，仅允许 HTTPS 的 `courseware/` 路径，不跟随重定向；生产 CDN 域名应在部署时核实。

导入会下载一次原课件，在当前环境 OSS 的 `knowledge/doc/` 创建独立快照，再走与上传相同的解析、教师校对、审核、索引和发布流程。原课件的签名链接到期、文件被替换或删除，不会破坏已发布知识资料；删除知识资料只删除快照和衍生资源，不删除原 `courseware/` 文件。

| 教师资料格式 | 当前处理与向量输入 |
| --- | --- |
| PDF | pypdf 分页提取文字，Poppler 渲染每页 JPEG；正文切片与同页图片一起送 Ark，保留页码。扫描页即使没有文字，也保留整页图片参与索引；没有自动 OCR |
| PPTX | `@office-kit/pptx` / preview 解析幻灯片、文字、图形和嵌入图片，SVG 经 resvg 生成 PNG；每页正文与预览图共同向量化。按演示文稿实际顺序保留页码 |
| DOCX | 受限 OOXML 提取正文与表格；当前不提供 Word 页面或嵌入图片渲染 |
| TXT / MD / CSV / HTML | UTF-8 解码与清洗后按正文切片；CSV 保留列名和行值，HTML 不执行脚本 |
| PNG / JPEG / WebP | 原图与可选教师说明联合向量化 |
| MP4 / MOV / AVI | 分段视频画面与教师核对字幕；抽帧用于回答引用，声音不会自动转写 |
| MP3 / WAV / OGG / FLAC / M4A | 必须提供核对后的字幕或转写，使用转写文本向量化 |
| DOC / PPT / XLS / XLSX | 暂不直接解析；分别转成 DOCX、PPTX、CSV，或导出 PDF 保留版面 |

Ark 接收解析后的文字、图片或视频，不直接接收 Office 文件。PDF/PPTX 每页文字按约 700 字切片，各片段配同页图片形成一个多模态语义单元；图像中的公式、图表和文字仍由模型理解，教师需要校对。后台“查看和校对”可按页查看预览图。校对正文创建新修订，继续保留对应页面图片。

PDF/PPTX 默认最多 200 页（`KB_DOCUMENT_MAX_PAGES` 可设 1–500），图片最长边 1600 像素，单份衍生图合计最多 100 MiB；两个环境共用单个渲染槽以限制内存。不支持或缺少处理能力时明确拒绝导入，不会悄悄降为仅文字。PPTX 代码渲染不保证与桌面 PowerPoint 完全相同；检测到暂不支持的 SmartArt/特殊图形或 Office 数学公式时提示先导出 PDF。旧版已上传的 PDF/PPTX 不会自动修改其发布修订，需要重新导入后审核发布，才能加入页面图片。

原件与每页预览图均保存到 OSS；本机临时目录仅用于解析。解析失败会回收本次新生成的页面对象，删除知识资料由持久任务一并清理原件、全部修订的页面图片和向量。学生聊天附件仍走题目正文解析流程，不自动进入教师图文知识库。

### 数学公式与源文件

PDF 文本能复制不代表公式结构完整。上下标、分数线、根号、矩阵和单位指数可能只剩字符位置；Unicode 解码正确也不能证明数学含义正确。当前保留同页图片和教师校对入口，不把普通文字提取声称为公式 OCR。生成与独立审核都收到文档页标志、页码和版式风险提示；已被生成模型看到且被引用的文档页，即使使用正文引句，也继续将原图送给审核。图片未提供或看不清时不得猜公式或据此计算。单次上下文有图片数量/尺寸预算，不保证整份文档的公式都能看清。

`.md` 已作为 UTF-8 正文支持。聊天气泡使用随前端打包的 KaTeX，支持 `\(...\)` / `$...$` 行内公式与 `\[...\]` / `$$...$$` 独立公式，包含上下标、分数、根号、矩阵和多行推导。生成提示词要求忠实使用 LaTeX，引用原文不变；不完整或错误公式回退到原文，不执行消息中的 HTML 或外部资源指令。部分历史反引号公式可兼容明确写出的符号，不推断缺失的幂次。排版不会提升 PDF 公式识别准确率，不能仅把扁平正文存成 MD 就认为公式已还原。

后续增强优先级：若 PDF 确实带 Formula/MathML/公式替代文本，先提取可用语义；否则用专门公式 OCR 生成 LaTeX 草稿，保留页码和原图供教师确认。可评估 Docling 公式增强或 Mathpix 等远程服务；尚未接入或为其配置密钥，不能视为已有能力。教师仍可只上传 PDF；可选附上原始 DOCX/LaTeX 源文件，有助于直接读取 OMML/LaTeX 结构。当前 DOCX 仅提取普通正文，尚未实现 OMML 转换。仅知道编辑软件名称不能恢复 PDF 已丢失的源公式。

## 模型配置

默认问答供应商是 DeepSeek：`https://api.deepseek.com`、`deepseek-flash`、Chat Completions、thinking enabled、reasoning_effort high。后台可修改供应商、地址、模型、图片理解、思考模式和强度。已保存的后台配置优先于 `.env` 默认值，管理员编辑页回显实际配置与可显隐的密钥；切换供应商必须填写新供应商密钥。

DeepSeek 使用 JSON object 模式，结构约束随提示词传入；随后仍做代码引用校验和独立模型审核。科学答案只能来自当前范围已发布资料，不启用搜索工具，不展示模型内部推理。现有渐进显示、阶段状态和耗时保留。思考模式会影响等待时间，可在后台调整。

向量模型仍为 Ark `doubao-embedding-vision-251215`，SQLite 保存结构与正文，Qdrant 提供带范围过滤的向量检索，FTS 补充文字召回。API 错误会区分密钥无效、余额不足、参数错误、限流和服务繁忙，用户可重试。后台保存的密钥加密后入 SQLite；`.env`、data 和 target 均忽略于 Git。

多轮追问读取用户消息、助教回答、引用和附件的关联；较早对话压缩时，另外保存有预算的来源 / 附件索引。每轮由简短的意图解析选择相关旧来源，再核对当前范围、发布修订和附件有效期，重新加载原文与图片。已保存的推荐问题直接恢复其支持来源，避免只拼接上一句用户文字。新附件不会自动带入无关的旧图片。

上传题图时，文字与图片分别向量检索并融合；已发布视频按问题补取多个时间帧，候选较多时先定位少量相关画面，再交给生成和一次独立审核。画面中的清晰文字走视觉审核，资料正文走精确引用校验；没有语音转写不意味着可读画面也不可解释。等待时间诊断分别记录意图、检索、取帧、画面定位、生成和审核。

DeepSeek 的 [图像理解](https://api-docs.deepseek.com/zh-cn/guides/vision/) 支持当前使用的 base64 图片输入。[Files API](https://api-docs.deepseek.com/zh-cn/guides/files_api/) 提供图片上传后复用 `file_id` 的方式；它不会自动检索课程资料，也不替代文档解析和 OSS。当前保持有效的 base64 协议。

`tests/e2e_context_live.py` 是按需运行的本地回归：需要环境变量中的学生测试账号，以及命令行指定的已发布人物图片、视频资源 ID 和题图路径。它创建自己的测试会话，验证隔轮图片追问、旧推荐问题及截图概念解释，结束时删除自己的会话和上传题图，不修改现有课程资料。

## 旧资源迁移

先停止本知识库实例并备份 SQLite、旧原件、Qdrant 和主密钥。迁移只处理数据库引用的文件，检查 SHA-256，相同内容跳过，同名异内容拒绝覆盖；不会更改发布状态、向量或删除旧原件。

```powershell
# 本机旧 data/local/knowledge.sqlite 与 data/local/uploads
.\target\debug\course-kb-server.exe migrate-storage --env-file .env --local-preview
.\target\debug\course-kb-server.exe migrate-storage --env-file .env --local-preview --apply
```

其他机器使用 `--environment dev|prod --source-dir <旧原件目录>`，数据库由相应配置确定。先不加 `--apply` 检查。2026-10-24 本工作区已备份 SQLite，并将 17 个被引用文件、65,364,997 bytes 上传到 futuredev 新路径并校验。旧 uploads 保留为迁移备份，运行服务改用独立空缓存。真实 OSS 上传、取回、Range 预览、删除已通过。

完整问答链路和证据边界见 [前端流程说明](../front_end/docs/知识库问答流程与资料管理.md)。本次仅本机开发与联调，不提交、不推送、不修改线上服务。
