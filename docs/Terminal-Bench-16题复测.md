# Terminal-Bench 2.1：16 题独立复测

## 已完成

- 固定复测上一轮的 9 道超时题和 7 道记录为有效 0 分的题；其中 OCaml 实际含判分环境故障。
- 新建独立任务副本和 12 个依赖镜像，另外 4 题沿用原镜像。没有修改 Agent 核心、旧成绩、原题或判分断言。
- 所需依赖在 Agent 解题前即可使用，而不只是判分时恢复。
- 模型：**deepseek/deepseek-flash**；运行时：**0.1.0-alpha.2**，源码指纹与产物检查通过。
- 解题时限 **6 倍**，判分时限 **3 倍**。这是扩时、预装依赖的诊断复测，不是官方时限成绩。
- 本次只运行准备和校验，尚未启动真实复测、未调用付费模型。

## 运行

在 WSL 终端执行：

~~~bash
cd /home/liuxuedeng/tb4-work/tb21-retest16
./run-retest16.sh preflight && ./run-retest16.sh run
~~~

默认最多同时 4 题，共享 16 GiB 内存、8 CPU 预算；按每题原资源需求排队，重题可能只有 1–2 题同时运行。每题显示阶段、用时、最近活动。

另开终端查看：

~~~bash
cd /home/liuxuedeng/tb4-work/tb21-retest16
./run-retest16.sh status --watch
~~~

降低并发可用 ./run-retest16.sh run --concurrency 2。不要重叠启动。Ctrl+C 停止本轮所属任务，旧轮不受影响。再次 run 创建新一轮，不是断点续跑。
预检不读取密钥、不调用模型；run 使用本机原有 DeepSeek 凭据。若源码自打包后改变，启动器会拒绝过期运行时。

## 逐题结论与修复

| 题目 | 原日志结论 | 本次处理 | 解题/判分上限（分钟） |
|---|---|---|---|
| dna-assembly | 初始缺 Python/pip、后续缺 Pillow，后来补装；最终失败来自提交内容约束。 | 预装 Python、primer3、Pillow、requests、PDF 工具。 | 180 / 90 |
| caffe-cifar-10 | 数据下载触及 600 秒命令超时，随后续传；判分 6 项通过，但 Agent 总时限超时。 | 预装构建库；缓存未修改的 Caffe 1.0 Git 源码和 CIFAR-10 原始数据，MD5 校验通过。不预训练模型。 | 360 / 60 |
| make-doom-for-mips | 超时并伴随 VM/运行时错误，没有合格画面；不只是下载慢。 | 预装 MIPS 编译器/二进制工具；执行逻辑仍由 Agent 修正。 | 90 / 45 |
| pytorch-model-cli | 缺编译器、torch/torchvision；实现的预处理改变了原模型推理语义。 | 预装 C/C++、CPU PyTorch 2.7.1、torchvision 0.22.1、NumPy/Pillow；原输入与判分不变。 | 90 / 45 |
| qemu-alpine-ssh | 初始缺 ps/free/file、pycdlib 等工具；最终 SSH 判分通过，但 Agent 超时。 | 预装进程/ISO 操作工具、sshpass、pycdlib；解决准备时遇到的旧版 Debian 包下载问题。 | 90 / 45 |
| extract-moves-from-video | 安装被命令超时中断，出现 dpkg 未完成状态；后续 OCR 长命令又超时，外部攻略检索也失败。 | 预装 ffmpeg、Tesseract、NumPy/Pillow；直接处理原视频，不提供攻略或答案。 | 180 / 90 |
| train-fasttext | 缺 C++ 编译器，旧 apt 索引导致 404；后来虽修复，持续调参仍超时，最终路径缺文件。 | 预装编译工具、fastText Python 0.9.3 和 CLI、进程工具；不提供训练模型。 | 360 / 180 |
| filter-js-from-html | 缺 curl/xxd；语料请求超时，本次核对原 URL 返回 404。另有正常 HTML 被改写和长时间模糊测试问题。 | 预装 html5lib/工具，缓存有效的 OWASP 公开参考文档；私有测试及原断言不变。 | 180 / 90 |
| fix-ocaml-gc | Agent 自测通过；判分阶段克隆失败后仍删除原测试目录，导致空输出。此前的 0 分含环境异常。 | 从未修改原镜像提取测试套件，改为判分时离线恢复；恢复失败先退出 97 并标记 OFFLINE-PREP-ERROR。 | 360 / 180 |
| gpt2-codegolf | 缺 Python/xxd，访问 Hugging Face 失败；权重已在镜像，官方存储可用。候选源码留在临时路径。 | 补工具，缓存原官方 encoder.json、model.ckpt.index、hparams.json；不提供实现或修改权重。 | 90 / 45 |
| password-recovery | 缺 xxd；主要耗时还包括过宽的文件系统扫描，最后没有提交结果。 | 补 xxd、file、进程工具；保留原始取证数据，不替 Agent 恢复答案。 | 90 / 45 |
| regex-chess | 依赖具备；替换规则错误，并通过模块遮蔽改变本地检查行为。 | 保留原镜像、题意和判分，只增加时间，不预装之前的规避实现。 | 360 / 180 |
| sanitize-git-repo | 无缺包致失败的证据；额外重写历史/清理对象删除了判分所需基准提交。 | 保留原镜像与 Git 数据，只增加时间。 | 90 / 45 |
| torch-tensor-parallelism | 无 Python/PyTorch；Agent 未尝试安装就用审阅代替实测，多进程梯度错误。 | 预装 CPU PyTorch 2.7.0、NumPy、pytest；1 CPU、断网条件下验证 2/4 进程基础通信与反向传播。 | 90 / 45 |
| tune-mjcf | 原判分 4 项通过，但达标后继续优化，最终超时。 | 原镜像、性能阈值不变，只增加阶段时限。 | 90 / 45 |
| video-processing | 初始缺 toml，后来补装；最终失败来自未见视频上的检测偏差。 | 预装 toml、ffmpeg；原视频和判分区间不变。 | 360 / 180 |

原 15/30/60 分钟解题时限变为 90/180/360 分钟。不修改题内速度、精度、模型大小等判分要求，也不修改 Agent 自己传给单次工具的 timeout。
环境和时间改动不保证算法、提交路径、过度优化问题自动消失。已处理已知必要依赖路径；可选外部检索站点不属于本次断网可用性保证。

## 验证结果

- 16/16 个解题环境依赖检查通过，均在断网容器中执行。
- 16/16 个原判分器的依赖恢复及测试收集通过；不是 16 题重新判分通过。
- PyTorch：GLOO_OK world_size=2 和 GLOO_OK world_size=4，退出 0。
- OCaml：缓存恢复与原镜像文件逐个校验一致；缓存缺失退出 97，不继续删除测试。
- 启动器 7 项离线单元测试通过；预检确认 16 题、6/3 倍参数。
- 主仓库静态检查各项通过。npm run check 的自动写入格式化被执行环境拦截；为保护已有修改，改为只读格式检查及其余全部检查项，没有执行写入。
- 另一份完整副本回滚测试通过：48 个原配置/说明/判分入口恢复，PyTorch 原镜像缺 Python 的退出码 127 得到复现；已准备的复测副本保持不变。

## 文件作用与结果位置

主仓库只新增本文档；依赖、脚本、验证记录放在独立目录，不堆入 docs。

| 绝对路径 | 作用 |
|---|---|
| /home/liuxuedeng/tb4-work/tb21-retest16/run-retest16.sh | 运行、预检和查看进度的日常入口（MODIFIED_FILE） |
| /home/liuxuedeng/tb4-work/tb21-retest16/retest.py | 固定 16 题、检查哈希、配置时限、复用现有并行调度器 |
| /home/liuxuedeng/tb4-work/tb21-retest16/manifest.json | 镜像 ID、源任务、依赖验证和文件 SHA-256 清单 |
| /home/liuxuedeng/tb4-work/tb21-retest16/tasks | 独立题目副本，保留原测试断言 |
| /home/liuxuedeng/tb4-work/tb21-retest16/task-changes.diff | 相对原题的完整改动（DIFF_FILE） |
| /home/liuxuedeng/tb4-work/tb21-retest16/VERIFICATION.txt | 原始、修改、回滚测试的实际命令/输出/退出码 |
| /home/liuxuedeng/tb4-work/tb21-retest16/ROLLBACK.sh | 在指定新比较副本恢复原配置，不覆盖原题或复测副本 |
| /home/liuxuedeng/tb4-work/tb21-retest16/original | 回滚所需的 48 个原配置/说明/判分入口 |
| /home/liuxuedeng/tb4-work/tb21-retest16/original-sha256.json | 原题关键文件及原判分包哈希 |
| /home/liuxuedeng/tb4-work/tb21-retest16/prepare.py | 准备脚本源码，已执行，无需重复运行 |
| /home/liuxuedeng/tb4-work/tb21-retest16/test_retest.py | 启动器离线单元测试 |
| /home/liuxuedeng/tb4-work/tb21-retest16/dependency-smoke.py | 不涉及题目实现的多进程环境测试 |
| /home/liuxuedeng/tb4-work/tb21-retest16/verify-graders.py | 断网恢复判分依赖和收集测试，不执行解题或计分 |
| /home/liuxuedeng/tb4-work/tb21-retest16/build | 镜像构建输入与依赖验证证据 |
| /home/liuxuedeng/tb4-work/tb21-retest16/runs | 真实复测启动后生成的新结果目录 |
| /home/liuxuedeng/tb4-work/tb21-retest16/latest-current-run.json | 真实复测启动后生成的新结果指针 |

每轮目录内：state.tsv 为逐题结果；summary.json 为汇总；run.json 记录模型、版本和有效时限。
Agent 日志位于新一轮的 jobs/<题名>-try1/.../<trial>/agent/agent-core.txt；判分日志位于对应 trial 的 verifier/test-stdout.txt。

原始轮次保留于：/home/liuxuedeng/tb4-work/tb21-offline/runs/current-full89-20260918-232218-pv5yf04v

[完整验证证据](file://wsl.localhost/Ubuntu-20.04/home/liuxuedeng/tb4-work/tb21-retest16/VERIFICATION.txt) · [改动对比](file://wsl.localhost/Ubuntu-20.04/home/liuxuedeng/tb4-work/tb21-retest16/task-changes.diff)
