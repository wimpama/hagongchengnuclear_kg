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
  logs/ai/dev/             开发环境 AI 输入、输出、检索与校验 trace（JSON）
  logs/ai/prod/            生产环境 AI trace（JSON）
  logs/                    其他本机日志
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

数据库迁移只追加新编号，已执行的 SQL 文件保持原始内容；SQLx 启动时会校验其 SHA-384，不能靠删除迁移记录或修改已存校验值绕过检查。第 11 号迁移保留最早已应用的原版，后续学习数据序号、身份确认和独立清理记录由第 12 号迁移升级，并保留原课程与聊天记录。迁移文件固定使用 LF 换行，`build.rs` 确保新增迁移会重新编译进二进制；Docker 构建也包含该文件。更新仍使用原来的部署命令，数据库会在服务启动时自动升级，不新增环境变量或 Nginx 配置。

Rust API 只映射宿主机 `127.0.0.1:8080`，Qdrant 只在 Compose 私网可达。Rust / Qdrant 默认各 0.5 CPU、768 MiB；共享的 2 vCPU / 4 GiB 服务器还运行原业务，实际并发能力需另行压测。可在别的 Linux 构建机执行 `docker compose build`，用 `docker save/load` 传镜像，再用 `sh start.sh --no-build` 启动。

两个站点的 TLS server 块都加入 `deploy/nginx-kb.conf` 中的 `/kb-api/` 片段，均代理到 8080，保留 `proxy_set_header Host $host`，保留现有 `/api`、`/ws` 和前端静态目录。不要整体覆盖原 Nginx 配置。模型推理、审核和压缩可能连续调用，片段设置了长连接读取超时并关闭代理缓冲。

日常查看：`docker compose -p course-knowledge logs --tail 100 knowledge-service`；Git 更新部署：`sh deploy.sh`；停止：`docker compose -p course-knowledge stop`。保留数据目录、OSS 原件、Qdrant 快照和主密钥备份，不使用删除卷的命令。当前开发机没有 Docker，Linux 镜像与线上部署仍待实际发布时验证。

每次问答还会在持久目录的 `logs/ai/dev/` 或 `logs/ai/prod/` 写一个 JSON trace，文件名包含开始时间、会话名称、用户 ID 和会话 ID。trace 包含检索片段、模型输入输出、校验结果和各阶段耗时；媒体 URL 与 data URL 会脱敏。该目录含课程正文和学生问题，只允许服务器管理员读取，不要通过 Nginx 暴露。

通过 Git 管理的服务器日常更新使用 `sh deploy.sh`，它会先拉取代码并构建，构建成功后替换容器；`start.sh` 适合启动已经准备好的本地源码 / 镜像。部署控制流程的离线回归可运行 `py -3 -m unittest discover -s tests -p test_deploy.py -v`；使用临时 Git 仓库和模拟 Docker，不连接服务器。

## 课程资料与已有课件导入

“同步课程结构”只更新原业务课程、一级和二级节点的镜像，用于确定资料归属与问答范围；不会导入课件、上传文件或调用向量模型。课程新增、改名、移动、删除后可同步，页面会显示同步数量。

知识库上传表单支持选择本地文件，或点击旁边的“从已有课件导入”，选择当前二级节点已有的课件；随后填写名称、可选说明和字幕并确认归属。后端只接受课件 ID，重新通过管理员接口核对归属和文件来源，不能导入其他节点课件或任意 URL。来源域名按 dev/prod 分别配置在 `KB_DEV_COURSEWARE_HOSTS` / `KB_PROD_COURSEWARE_HOSTS`，仅允许 HTTPS 的 `courseware/` 路径，不跟随重定向；生产 CDN 域名应在部署时核实。

导入会下载一次原课件，在当前环境 OSS 的 `knowledge/doc/` 创建独立快照，再走与上传相同的解析、教师校对、审核、索引和发布流程。原课件的签名链接到期、文件被替换或删除，不会破坏已发布知识资料；删除知识资料只删除快照和衍生资源，不删除原 `courseware/` 文件。

| 教师资料格式 | 当前处理与向量输入 |
| --- | --- |
| PDF | pypdf 分页提取文字，Poppler 渲染每页 JPEG；正文切片与同页图片一起送 Ark，保留页码。扫描页即使没有文字，也保留整页图片参与索引；没有自动 OCR |
| PPTX | `@office-kit/pptx` / preview 解析幻灯片、文字、图形和嵌入图片，SVG 经 resvg 生成 PNG；每页正文与预览图共同向量化。按演示文稿实际顺序保留页码 |
| DOCX | 受限 OOXML 提取正文、表格和 Word 原生 OMML 公式；当前不提供 Word 页面或嵌入图片渲染 |
| TXT / MD / CSV / HTML | UTF-8 解码与清洗后按正文切片；CSV 保留列名和行值，HTML 不执行脚本 |
| PNG / JPEG / WebP | 原图与可选教师说明联合向量化 |
| MP4 / MOV / AVI | 分段视频画面与教师核对字幕；抽帧用于回答引用，声音不会自动转写 |
| MP3 / WAV / OGG / FLAC / M4A | 必须提供核对后的字幕或转写，使用转写文本向量化 |
| XLSX / XLS | 可选“直接证据”或“其中文本作为证据”；前者不读取单元格、不生成内容向量，后者读取可见工作表文字、数值及已有缓存结果。XLSX 同时保留公式源码，不执行函数、宏或外部链接 |
| DOC / PPT | 暂不直接解析；分别转成 DOCX、PPTX，或导出 PDF 保留版面 |

Ark 接收解析后的文字、图片或视频，不直接接收 Office 文件。PDF/PPTX 每页文字按约 700 字切片，各片段配同页图片形成一个多模态语义单元；图像中的公式、图表和文字仍由模型理解，教师需要校对。后台“查看和校对”可按页查看预览图。校对正文创建新修订，继续保留对应页面图片。

PDF/PPTX 默认最多 200 页（`KB_DOCUMENT_MAX_PAGES` 可设 1–500），图片最长边 1600 像素，单份衍生图合计最多 100 MiB；两个环境共用单个渲染槽以限制内存。不支持或缺少处理能力时明确拒绝导入，不会悄悄降为仅文字。PPTX 代码渲染不保证与桌面 PowerPoint 完全相同；检测到暂不支持的 SmartArt/特殊图形或 Office 数学公式时提示先导出 PDF。旧版已上传的 PDF/PPTX 不会自动修改其发布修订，需要重新导入后审核发布，才能加入页面图片。

原件与每页预览图均保存到 OSS；本机临时目录仅用于解析。解析失败会回收本次新生成的页面对象，删除知识资料由持久任务一并清理原件、全部修订的页面图片和向量。学生聊天附件仍走题目正文解析流程，不自动进入教师图文知识库。

### 片段编辑与 Excel 资源

“查看与确认”默认直接编辑排版后的内容，不再提供重复的阅读预览。支持增删片段、选择新增片段类型，正文可直接改字；公式保持 KaTeX 排版，双击公式单独修改，特殊格式可切到“源码（高级）”。媒体类型只允许选择原件实际具备的类型，不能把普通文档标成视频。视频/音频使用时、分、秒输入（秒保留至毫秒），显示原件总时长；起止全部留空表示完整媒体，新增视频片段同样支持。指定范围必须同时填写两端，满足 0 ≤ 开始 < 结束 ≤ 原件总时长，前后端均校验。清空已裁剪视频的范围会恢复原视频，审核时重新处理，不复用原短片冒充完整视频。批量保存一次生成一个新修订，校验失败整体回滚；旧的在线修订仍可访问，待新版本审核/索引后发布。

后台通过 `GET /kb-api/v1/admin/knowledge/assets/{id}/media-metadata` 获取原件时长。新上传的媒体在解析时记录可信时长；旧媒体首次读取时从 OSS/处理缓存取回并用 ffprobe 核实，随后复用原件元数据，不在 SQLite 写事务中等待网络。原件缺失或读取超时会明确报错。

视频“审核通过”包含裁剪分段、抽取画面、上传衍生素材和保存审核结果，仍需实际处理时间。资料列表每 3 秒读取当前阶段，同时显示已耗时；原件已在 OSS，不会在每次审核时重复上传。Rust 日志分别记录媒体处理、衍生素材上传和审核总耗时。重复审核遇到正在处理的任务会立即返回可重试提示；服务重启后，中断的审核显示为可重试状态。索引工作线程在空闲时释放编辑锁，忙时保存返回明确提示，不让请求无限排队。这些改动不新增环境变量或数据库迁移。

Excel 默认作为直接证据：教师确认并发布后，可通过文件名和补充说明检索到资源，学生在页面内查看原表格；不将文件的函数执行结果当作模型已计算的事实。文字证据模式需要 `processing/requirements.txt` 中的 openpyxl、xlrd、defusedxml；Docker 已包含，本机可用所选 Python 执行 `-m pip install -r processing/requirements.txt`。XLSX 提取公式源码和缓存值，XLS 仅提取值/缓存结果，不自动执行任何公式或宏；此模式不是工作表截图 OCR。过大或异常文件会明确报错，不静默截掉后半部分。两种模式的原件均写入 OSS，撤回和删除遵循同一权限与清理链路。

XLS/XLSX 页内预览复用现有 PPT 的微软 Office Viewer。`POST /kb-api/v1/knowledge/assets/{asset_id}/office-preview` 先复用学生身份、课程权限、片段归属和发布检查，再签发仅对应原文件的 OSS V4 GET 链接（10 分钟）；学生附件不能走此入口。前端在固定的 `view.officeapps.live.com` iframe 中打开，不强制下载，不新增表格编辑器、公式计算引擎或 Office 安装依赖。有效链接可手动刷新，教师说明另行折叠显示。微软服务会读取并可能缓存该文件，已签发链接到期前不能按课程撤回状态即时吊销；撤回后不再签发新链接，物理删除原件会阻止再次从 OSS 获取。在线预览的可用性与文件兼容性由微软服务决定，浏览器测试使用模拟预览服务，不声称完成微软真实渲染联调。

学生查看文字依据与后台编辑共用 Markdown/LaTeX 渲染，支持标题、加粗、列表、代码及表格；不执行原文中的 HTML。原始 Markdown 仍保存在数据库供索引、精确引用和高级编辑使用。

### 教学 AI 助手

后台课程列表、课程、一级节点和二级节点页右上角提供“教学 AI 助手”。它共享后台问答/向量模型配置，分析该范围内学生已完成的问答；统计由 SQL 计算，语义检索只提供相关样本，不能把样本人数冒充全量，也不能把提问等同于“未掌握”。回复支持 Markdown 表格与 LaTeX，流式显示并记录耗时，可查看参考的原始问答样本。

Rust 启动时自动启动同步任务，空闲每分钟检查，积压时分批继续。学生问答副本进入独立的 `kb_learning_*` 表和 `learning_dev_*` / `learning_prod_*` Qdrant 集合；不会进入学生课程问答的权威知识库。新会话记录鉴权身份，管理员试问不计入学生统计；旧记录没有身份标签，学生再次访问课程助教后补记身份并同步，数据管理页显示未确认的历史会话数量。课程层会话不自动归到每一个子章节。

数据管理可查看同步时间、最新记录、问答体量、向量体量及待索引数。问答体量为正文和引用的 UTF-8 字节；向量体量为维度×4字节的原始浮点向量估算，不含 Qdrant 索引和元数据开销。可分别清理当前范围的会话分析副本和向量；不会删除学生自己的聊天历史或课程资料，已清理旧历史不会在下一次同步时自动回填，新问答继续同步。清理会话副本后，独立保留的向量只计量，不在缺少原文时用于回答；清理向量会等待 Qdrant 确认。学生删除自己的会话后，分析副本与向量由后续同步回收。

教师分析历史只对会话所属管理员可见，存于独立表；日志沿用现有目录并增加 `teacher/` 子目录。此功能不需要增加 `.env` 配置，更新后重启 Rust 服务会自动执行数据库迁移。

教学助手按教师的具体问题组织回答，不强制输出全量统计表。代码提供去重的问题组、不同学生集合和近两周时间分布，模型归纳相近语义主题、反复追问及教学建议，明确统计覆盖范围。问得多不直接等于没掌握，问得少不等于学会。只有回答实际引用的问答样本才展示为依据，使用 `[记录N]` 定位，不显示内部 UUID。

学生基本信息从当前环境的身份接口及管理员用户详情接口获取，缓存于本服务对应环境的 `kb_student_profiles`，不把内部用户 ID 当作学号；姓名、学号、班级、学院、专业和年级缺失时如实标明。管理员详情查验使用当前教师的令牌，半小时内复用缓存，资料补全不要求历史学生重新登录。新增迁移 `0013_student_profiles.sql`，不修改已应用的迁移；无需更改环境变量。

### 数学公式与源文件

PDF 文本能复制不代表公式结构完整。上下标、分数线、根号、矩阵和单位指数可能只剩字符位置；Unicode 解码正确也不能证明数学含义正确。当前保留同页图片和教师校对入口，不把普通文字提取声称为公式 OCR。生成与独立审核都收到文档页标志、页码和版式风险提示；已被生成模型看到且被引用的文档页，即使使用正文引句，也继续将原图送给审核。图片未提供或看不清时不得猜公式或据此计算。单次上下文有图片数量/尺寸预算，不保证整份文档的公式都能看清。

`.md` 已作为 UTF-8 正文支持。聊天气泡使用随前端打包的 KaTeX，支持 `\(...\)` / `$...$` 行内公式与 `\[...\]` / `$$...$$` 独立公式，包含上下标、分数、根号、矩阵和多行推导。生成提示词要求忠实使用 LaTeX，引用原文不变；不完整或错误公式回退到原文，不执行消息中的 HTML 或外部资源指令。部分历史反引号公式可兼容明确写出的符号，不推断缺失的幂次。排版不会提升 PDF 公式识别准确率，不能仅把扁平正文存成 MD 就认为公式已还原。

后续增强优先级：若 PDF 确实带 Formula/MathML/公式替代文本，先提取可用语义；否则用专门公式 OCR 生成 LaTeX 草稿，保留页码和原图供教师确认。可评估 Docling 公式增强或 Mathpix 等远程服务；尚未接入或为其配置密钥，不能视为已有能力。教师仍可只上传 PDF；可选附上原始 DOCX/LaTeX 源文件。Word 原生 DOCX 公式会读取 OMML 的上下标、分式、根号和多行结构并转成 LaTeX，同时保留原文件和教师校对入口；仅知道编辑软件名称不能恢复 PDF 已丢失的源公式。

## 模型配置

默认问答供应商是 DeepSeek：`https://api.deepseek.com`、`deepseek-flash`、Chat Completions、thinking enabled、reasoning_effort high。后台可修改供应商、地址、模型、图片理解、思考模式和强度。已保存的后台配置优先于 `.env` 默认值，管理员编辑页回显实际配置与可显隐的密钥；切换供应商必须填写新供应商密钥。

校验开启时，DeepSeek 使用 JSON object 模式生成课程依据草稿，随后执行代码引用校验和一次独立模型审核。校验关闭时，同样先检索，再按提示词组织讲解并直接流式输出。两种方式都聚焦学生问题，不主动汇报知识库或系统资源清单；不启用联网工具，不展示模型内部推理。思考模式会影响等待时间，可在后台调整。

只有回答实际使用的来源才展示：直出模式按模型的段落引用标记建立依据关系，引用模式把预览放在对应段落，下挂模式仅列实际使用的条目，未引用的检索候选不展示。关闭溯源只影响显示，实际引用关系仍保存供历史追问校核。关闭校验时的引用由模型声明、代码核对编号和当前发布状态，不表示做过独立的逐句事实审核。

没有足够依据、没有可读证据或独立审核不通过时，不再附送候选资源清单，也不将未通过的草稿交给学生。单独生成温和的参考解答，并由代码添加“仅 AI 输出，无溯源证据，仅供参考。”；可讲一般概念和思路，不能猜人物身份、课程实验是否存在、未清晰给出的公式或参数。该回答带持久的非权威标记，可重载与幂等重试；历史只用来理解讨论关系，不自动变成课程知识。模型/检索故障仍返回可重试错误，不伪装成资料不足。

向量模型仍为 Ark `doubao-embedding-vision-251215`，SQLite 保存结构与正文，Qdrant 提供带范围过滤的向量检索，FTS 补充文字召回。API 错误会区分密钥无效、余额不足、参数错误、限流和服务繁忙，用户可重试。后台保存的密钥加密后入 SQLite；`.env`、data 和 target 均忽略于 Git。

多轮追问读取用户消息、助教回答、引用和附件的关联；较早对话压缩时，另外保存有预算的来源 / 附件索引。每轮由简短的意图解析选择相关旧来源，再核对当前范围、发布修订和附件有效期，重新加载原文与图片。已保存的推荐问题直接恢复其支持来源，避免只拼接上一句用户文字。新附件不会自动带入无关的旧图片。

上传题图时，文字与图片分别向量检索并融合；已发布视频按问题补取多个时间帧，候选较多时先定位少量相关画面，再交给生成和一次独立审核。画面中的清晰文字走视觉审核，资料正文走精确引用校验；没有语音转写不意味着可读画面也不可解释。等待时间诊断分别记录意图、检索、取帧、画面定位、生成和审核。

DeepSeek 的 [图像理解](https://api-docs.deepseek.com/zh-cn/guides/vision/) 支持当前使用的 base64 图片输入。[Files API](https://api-docs.deepseek.com/zh-cn/guides/files_api/) 提供图片上传后复用 `file_id` 的方式；它不会自动检索课程资料，也不替代文档解析和 OSS。当前保持有效的 base64 协议。

教师助手的来源展示共享问答模型配置：严格校验对应段内引用；关闭校验时跟随依据溯源的引用、下挂或隐藏选项。模型的 `[记录N]` 仅用于建立段落与问答的关联，页面转换成可点击卡片，不直接展示内部编号。只列实际使用的记录，历史和幂等重试也读取当前配置。

学生问答的学情索引包含当轮上传的原图和可提取文档正文，仍使用独立 `learning_*` 集合，绝不进入教师发布的课程知识库。新增迁移 0014 保留附件 ID；每次读取重新核对会话、教学范围、附件归属及 30 天有效期，教师通过专用管理员接口预览原图/文档。原件仍存于 OSS `knowledge/user_upload/`，不产生第二份永久文件；历史问答自动补录有效附件，附件过期后替换对应向量以去掉附件输入。模型每轮最多接收 6 张原图，未提供的图片明确标记，避免用过去助教的描述冒充已看过原图。不新增环境变量，不改写已应用迁移。

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
