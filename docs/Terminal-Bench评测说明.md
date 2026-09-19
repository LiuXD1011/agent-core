# Terminal-Bench 2.1 本地评测

当前入口适配 `packages/agent-app`、原生 `context.json` 配置及当前 SDK；外部评测资产独立于主仓库。源码变化后必须重新打包。不会沿用旧运行时或旧轮次的完成标记。

## 默认行为

- 模型固定为 `deepseek/deepseek-flash`，沿用本机模型目录定义。
- 使用离线映射中的 **69 题**。镜像和判分依赖预置，不表示 Agent 无需模型网络，也不保证题目内部完全不访问网络。
- 三题冒烟：`chess-best-move`、`bn-fit-modify`、`caffe-cifar-10`。
- 最多 **4 题并行**，累计声明资源不超过 **16 GiB 内存、8 vCPU**；8 GiB 重型题最多同时 2 题。三题冒烟也参与并行调度，不是串行。此为保守预算，不是设备极限测量结论。
- Agent 与 verifier 各自采用本地任务声明的 **1 倍时限**，没有修改题目的 CPU/内存配置。
- 上下文优化随 Agent 主流程工作：打包当前 `.agent-core/context.json`；诊断精简沿用当前会话模型。没有额外插件或独立功能启动步骤。
- 每题显示阶段条、已用时、最近输出时间和心跳；阶段条不表示解题完成百分比。
- 每题每轮只跑一次，不自动重试超时、0 分或环境错误。每轮创建新目录，不自动恢复旧轮次。
- 新版入口使用互斥锁，避免同时启动两轮新版测试。已有其他评测容器时默认拒绝启动，不会替用户终止旧容器。

## 1. 打包当前源码

在 WSL 终端运行：

```bash
cd /home/liuxuedeng/src/agent-core
export PATH="$HOME/.nvm/versions/node/v22.23.2/bin:$HOME/.local/bin:$PATH"

npm run check &&
node /home/liuxuedeng/tb4-work/agentcore-adapter/prepare-runtime.mjs
```

打包脚本先执行当前项目的 `build:offline`，再打包公开工作区包、以 `--ignore-scripts` 安装独立消费者、验证 SDK 和 CLI。随后生成可搬迁的 Node + Agent 运行时，把当前上下文配置和模型目录一并打包；不会复制宿主的认证文件、会话或扩展。

解压自检会验证版本、CLI 哈希、配置哈希、模型注册和上下文配置。只有全部通过，才更新：

`/home/liuxuedeng/tb4-work/agentcore-runtime/latest.json`

看到 `PACKAGING_OK` 才进入下一步。旧运行时目录保留。构建和安装可能使用本机 npm 缓存或软件源，但不会发起模型请求。

## 2. 预检与三题冒烟

```bash
cd /home/liuxuedeng/tb4-work/agentcore-adapter
./run-tb21.sh preflight &&
./run-tb21.sh smoke3
```

预检核查全 69 题映射、离线判分包 SHA-256、本地镜像、当前源码与运行时指纹以及凭据是否可用；不发起模型请求。实际冒烟会调用模型并判分。

凭据优先使用已有环境变量 `DEEPSEEK_API_KEY`，否则读取本机 `~/.agent-core/agent/auth.json` 的 deepseek key；密钥不写入启动参数或轮次元数据。如需手动设置：

```bash
read -rsp 'DeepSeek API key: ' DEEPSEEK_API_KEY
echo
export DEEPSEEK_API_KEY
```

每题容器安装运行时后再次自检，并用 `agent-core --version` 回填 Harbor 的实际 Agent 版本。冒烟的三题均正常完成判分（合法 0 分也属于正常判分）后，才允许相同运行时、适配器和时限进入全量测试。查看 0 分题的 verifier 日志，不将其误认成安装故障。

## 3. 运行全部 69 题

确认冒烟结果后，单独执行：

```bash
cd /home/liuxuedeng/tb4-work/agentcore-adapter
./run-tb21.sh full
```

另开终端查看：

```bash
cd /home/liuxuedeng/tb4-work/agentcore-adapter
./status.sh --watch
```

指定某一轮（包括使用同样进度格式的历史轮次）：

```bash
./status.sh --run-dir /绝对路径/本轮结果目录 --watch
```

不要使用旧的 `xargs -P` 再套一层并行，新入口已经负责资源调度。需要降低负载可用 `--concurrency 2`；题目的时间和资源声明不变。

## 4. 新增 20 题：与原 69 题统一并行

新增 **20/20 题判分依赖已准备完成**，在全新原始镜像、关闭网络的容器中通过恢复安装、测试收集及必要的数据/模型加载验证。这不是 20 题解题通过率；真实 Agent 解题与判分仍需启动。

- 额外资产独立放在 /home/liuxuedeng/tb4-work/tb21-extra/；原 69 题映射、依赖包和入口不变。
- 原始说明、测试断言、CPU/内存和时限均未修改，仅将判分依赖安装改为本地来源。C4 离线缓存别名指向同一份原分片，不替换数据。
- 仅补充判分资产不需要重新打包或重跑已完成的同版本冒烟；以后修改 Agent 源码仍需重新打包并冒烟。
- 判分包在 verifier 阶段恢复，不额外给 Agent 提供答案或测试文件。模型调用和题目解题过程仍可能需要网络。

### 推荐：一个 89 题队列

等正在运行的评测结束，再执行：

~~~bash
cd /home/liuxuedeng/tb4-work/agentcore-adapter
./run-tb89.sh preflight &&
./run-tb89.sh full
~~~

它交错排列原 69 题与新增 20 题，共享最多 **4 题、16 GiB 内存、8 vCPU** 的并行预算，保持各题 **1 倍时限**。大资源题可能等待空位，不保证始终有 4 题同时运行。

预检验证 89 题资产、哈希、本地镜像及当前运行时，不调用模型；实际运行使用 deepseek/deepseek-flash，沿用同版本三题冒烟门禁和互斥锁。

如果原 69 题已经全部跑完，只补测 20 题：

~~~bash
cd /home/liuxuedeng/tb4-work/agentcore-adapter
./run-tb89.sh extra20
~~~

另开终端查看进度：

~~~bash
cd /home/liuxuedeng/tb4-work/agentcore-adapter
./status.sh --watch
~~~

结果仍在 /home/liuxuedeng/tb4-work/tb21-offline/runs/：89 题轮次为 current-full89-*，单独 20 题为 current-extra20-*，不会覆盖旧结果。若 69 题全量已开始，让它结束后补测，或者自行停止后改用统一 89 题队列；不要叠加两个独立调度器的资源预算。

### 新增资产清单

每题同时包含原脚本要求的 Python/pytest 缓存。下表列出主要特殊依赖，压缩包合计约 3.39 GiB。

| 题目 | 主要本地依赖 | 压缩包 MB | 断网验证 |
| --- | --- | ---: | --- |
| build-pov-ray | ImageMagick 图像比较 | 135.4 | 通过 |
| compile-compcert | binutils / readelf | 68.7 | 通过 |
| dna-assembly | primer3 / oligotm | 64.6 | 通过 |
| dna-insert | primer3 / oligotm | 64.6 | 通过 |
| extract-elf | GCC 与 ELF 检查依赖 | 59.8 | 通过 |
| feal-differential-cryptanalysis | GCC、Python 扩展编译 | 134.1 | 通过 |
| git-multibranch | expect 交互控制 | 61.0 | 通过 |
| headless-terminal | Vim、pytest、requests 本地 wheel | 70.2 | 通过 |
| install-windows-3.11 | mtools、socat、vncsnapshot、Tesseract | 157.6 | 通过 |
| mailman | Python 3.12、Mailman 3.3.8 | 79.5 | 通过 |
| make-doom-for-mips | Pillow 图像校验 | 98.5 | 通过 |
| make-mips-interpreter | Pillow 图像校验 | 90.9 | 通过 |
| merge-diff-arc-agi-task | Git 及 Python 判分依赖 | 82.0 | 通过 |
| modernize-scientific-stack | 原测试声明的科学计算依赖 | 151.4 | 通过 |
| pytorch-model-cli | PyTorch 2.7.1 CPU、torchvision、OpenCV、FFmpeg、MNIST | 555.4 | 通过 |
| qemu-alpine-ssh | sshpass | 58.9 | 通过 |
| qemu-startup | expect | 58.8 | 通过 |
| reshard-c4-data | datasets 3.6.0、原 C4 第 00009 分片 | 796.2 | 通过 |
| sam-cell-seg | PyTorch 2.5.1 CPU、固定提交 MobileSAM、原模型权重 | 638.7 | 通过 |
| train-fasttext | 本地编译 fastText CLI、编译工具链 | 214.3 | 通过 |

MNIST 断网加载得到 10,000 条测试数据；C4 原分片得到 356,318 条记录；MobileSAM 权重加载得到 439 个状态项。fastText 的实际编译提交号保存在其判分包内的 fasttext-commit.txt。

新增文件的作用：

| 位置 | 作用 |
| --- | --- |
| /home/liuxuedeng/tb4-work/agentcore-adapter/run-tb89.sh | 89 题/20 题用户入口 |
| /home/liuxuedeng/tb4-work/agentcore-adapter/tb21_extended.py | 资产校验、交错队列，复用既有监督器 |
| /home/liuxuedeng/tb4-work/tb21-extra/engine/prepare_extra.py | 下载、打包、断网验证判分依赖 |
| /home/liuxuedeng/tb4-work/tb21-extra/engine/test_prepare_extra.py | 离线转换和扩展队列回归测试 |
| /home/liuxuedeng/tb4-work/tb21-extra/manifest.json | 逐题状态、哈希、镜像 ID、大小和验证退出码 |
| /home/liuxuedeng/tb4-work/tb21-extra/task_paths.extra.tsv | 新增 20 题映射 |
| /home/liuxuedeng/tb4-work/tb21-extra/tasks/ | 隔离的题目副本及判分包 |
| /home/liuxuedeng/tb4-work/tb21-extra/logs/ | 逐题依赖准备和断网验证日志 |

复验新增依赖，不调用模型：

~~~bash
cd /home/liuxuedeng/tb4-work/tb21-extra/engine
"$HOME/.local/share/uv/tools/harbor/bin/python" -m unittest -v test_prepare_extra
"$HOME/.local/share/uv/tools/harbor/bin/python" prepare_extra.py --validate-only --workers 2
~~~

## 旧容器与停止

启动前可用 docker ps 查看已有评测容器。入口不会自动停止其他轮次；不要用两个独立队列叠加资源预算。

新版测试中按 Ctrl+C 会停止继续派题、通知当前子进程，并尝试停止**本轮有记录的试题容器**；不会停止旧轮次的容器。保留已写日志和原始结果，当前题记为 cancelled。再次运行会新开一轮，不会自动续跑。

## 结果与判分口径

每轮目录为：

`/home/liuxuedeng/tb4-work/tb21-offline/runs/current-<模式>-<时间>-<随机后缀>/`

| 文件 | 作用 |
| --- | --- |
| `run.json` | 本轮模型、运行时版本与哈希、源码指纹、资源预算、逐题时限和任务清单 |
| `state.tsv` | 逐题状态、本地统计分数、Harbor 原始 reward、异常类型、原始 trial 路径 |
| `results.json` | 机器可读逐题明细 |
| `summary.json` | 完成数、通过数、有效判分数、超时和错误计数 |
| `jobs/<任务>-try1/progress.json` | 单题阶段和心跳 |
| `jobs/<任务>-try1/launch.log` | 单题 Harbor 启动日志 |
| `jobs/.../<trial>/result.json` | Harbor 原始结果，保持不改写 |
| `jobs/.../<trial>/verifier/test-stdout.txt` | 实际判分输出 |

- `valid`：版本一致、有完成结果与判分输出、没有异常，reward 可以是 0。
- `timeout`：Agent 或 verifier 超时；本地严格统计记 0，另存原始 reward，不把超时后的 reward=1 计为通过。这是本入口的统计规则，不改写 Harbor 原始结果。
- `infra`：离线依赖、缺少判分证据、版本不符等问题，不伪装成正常 0 分。
- `error` / `cancelled`：其他运行异常或中断，单独记录。

69 题子集或扩展后的 89 题离线改编集都不自动等同于官方成绩。1 倍时限也不使它自动具备官方排行榜可比性。要复现之前的 2 倍开发预算，冒烟和全量都显式加 `--timeout-multiplier 2`，并将结果标记为扩展预算。

## 维护入口

这些文件位于 `/home/liuxuedeng/tb4-work/agentcore-adapter/`，不进入 Agent 核心：

| 文件 | 作用 |
| --- | --- |
| `prepare-runtime.mjs` | 重建源码、消费者打包、原生配置集成、运行时自检与发布本地最新指针 |
| `agent_core_tb.py` | Harbor 容器安装、运行时校验、模型凭据环境注入和 CLI 调用 |
| `tb21_local.py` | 资产预检、预算并行调度、逐题结果分类和取消处理 |
| `run-tb21.sh` | 用户运行入口 |
| `run-tb21-guarded.sh` | 原命名任务入口，统一交给新版监督器 |
| `tb21_progress.py` | 单题进程监控、阶段和心跳记录 |
| `status.sh` | 查看最新或指定轮次的逐题状态 |
| `test_tb21_local.py` | 离线调度、适配器和判分回归测试 |
| `test_prepare_runtime.mjs` | 配置校验与运行时目录组装测试 |
| `test_tb21_progress.py` | 既有进度与信号转发测试 |

仅运行离线回归：

```bash
cd /home/liuxuedeng/tb4-work/agentcore-adapter
"$HOME/.local/share/uv/tools/harbor/bin/python" -m unittest -v test_tb21_local test_tb21_progress
"$HOME/.nvm/versions/node/v22.23.2/bin/node" --test test_prepare_runtime.mjs
```
