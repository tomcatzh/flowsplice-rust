# FlowSplice 0.2 第二 Home macOS Quick Start

这份包用于一台空白的 Apple Silicon Mac。Home 1 与 Home 2 使用同一个、与具体部署无关的 `flowsplice-homeagent`。Home ID 和权限在远程批准时生成，不需要手工准备运行期证书、私钥或 `homeagent.toml`。

## 运行前条件

- 包内的二进制不绑定根公钥、CA、Server ID、证书名、端口或 IP；这些值全部位于独立的 `home-bootstrap.toml`、`deployment-root.pub` 和根签名 `deployment-trust.json`。
- 新 Mac 仍然只输入 Server IP。程序自动读取同目录配置，先验证 deployment trust，再从已验证 trust 中取得 Management CA。
- Server 的 Home control 端口（默认 `7443`）可从新 Mac 主动访问。
- 至少一台已有的 Global issuer Home 在线，操作员可以打开它的本地 Home 页面并输入签发密码。
- 新 Home 不需要公网入站端口；批准页面也不需要在新 Mac 上远程打开。

## 1. 解包并验证

```bash
tar -xzf flowsplice-home2-0.2.0-macos-arm64.tar.gz
cd flowsplice-home2-0.2.0-macos-arm64
shasum -a 256 -c SHA256SUMS
codesign --verify --strict --verbose=2 ./bin/flowsplice-homeagent
codesign -dvvv ./bin/flowsplice-homeagent 2>&1 \
  | grep 'Identifier=io.zxf.flowsplice.homeagent'
```

包内二进制使用免费的 ad-hoc codesign。它不是 Developer ID 签名，也没有 notarization；`SHA256SUMS` 用于校验发布文件，ad-hoc 签名用于校验 Mach-O 没有在签名后被修改。

这两种校验都不能证明发布者身份。首次使用前，必须通过另一个可信渠道核对整个包的 SHA-256，或至少核对 `deployment-root.pub` 的 SHA-256 指纹；不要只依赖同一个下载包内的 `SHA256SUMS`。

解包后的包结构必须是：

```text
flowsplice-home2-0.2.0-macos-arm64/
├── bin/flowsplice-homeagent
├── home-bootstrap.toml
├── deployment-root.pub
├── deployment-trust.json
├── QUICK_START.zh-CN.md
└── SHA256SUMS
```

不要把这三个配置/信任文件移出包目录。改变 Server 身份、端口或 trust 时只替换经过验证的新配置文件，不重新编译 HomeAgent。

## 2. 新 Mac 只运行这一条命令

把 `<SERVER_IP>` 换成 Server 的 IP：

```bash
./bin/flowsplice-homeagent init --server <SERVER_IP>
```

命令会显示随机生成的 Home ID 和校验码，然后停在：

```text
Waiting for approval on any online global Home page...
```

这是正常状态。不要关闭终端，也不需要 SSH 转发、远程打开新 Home 页面或增加其他 TCP 参数。

## 3. 在另一台已有 Global Home 上批准

打开任意一台在线 Global issuer Home 的本地页面：

1. 展开“新 Home 申请”。
2. 核对 Home ID 和校验码与新 Mac 终端完全一致。
3. 选择有效期和以下一种权限：
   - **Serving-only**：只运行 Home 业务和本地统计，没有 Travel 签发、撤销或新 Home 批准能力。
   - **Home issuer**：除运行业务外，只能签发本 Home 或本 Home 指定业务范围的 Travel 凭据。
   - **Global issuer**：还可以签发全局 Travel 凭据并批准以后加入的 Home。
4. 输入这台批准 Home 的签发密码并确认。

如果多台 Global Home 同时在线，Server 会把同一申请发给全部 Global Home；第一份通过完整密码和签名校验的批准生效，重复或迟到的结果不会创建第二个身份。

对于 Home issuer 或 Global issuer，批准时使用的签发密码也是新 Home 上发行私钥的解密密码。操作员必须按生产密码管理规则保存它；密码不会写入 TOML，也不会发送给 Relay 或 Travel。

## 4. 批准后自动完成的内容

新 Mac 上的 `init` 会自动：

- 生成独立的 Management/Business 私钥和 CSR；私钥从不离开新 Mac；
- 读取 `home-bootstrap.toml`，验证 `deployment-trust.json` 的根签名、有效期、部署 ID 和 generation；
- 通过新 Home 主动建立的 Server control 连接取得批准结果；Server 再把申请转发给已在线的 Global Home；
- 校验部署根签名、Home 端点签名、证书身份、SPKI 和有效期；
- 安装双叶证书、双 CA、公有 deployment trust 和 Home 端点凭据；
- 根据批准范围安装零把、Home 范围或全局范围的加密发行私钥；
- 生成 `homeagent.toml` 和 `state/home-state.redb`；
- 记录实际 TLS 握手得到的 Server SPKI；
- 把当前二进制复制到固定安装目录；
- 创建当前用户的 launchd 服务并立即启动。

安装目录为：

```text
~/Library/Application Support/FlowSplice/Home/
├── bin/flowsplice-homeagent
├── cert/
├── issuer/                 # Serving-only 不存在发行私钥
├── state/home-state.redb
└── homeagent.toml
```

launchd 文件位于：

```text
~/Library/LaunchAgents/io.zxf.flowsplice.homeagent.<HOME_ID>.plist
```

成功后终端会给出本地页面地址，默认是：

```text
http://127.0.0.1:9082/
```

统计数据默认不读取；只有点击第二页 Statistics 后才查询 redb 中的五分钟汇总和日/周/月/年报表。

## 5. 业务服务配置

身份、证书、权限、状态库和常驻进程全部由一条 init 命令完成。程序不会猜测这台 Mac 上实际运行的业务，因此初始 `services` 为空。需要发布业务时，在生成的 `homeagent.toml` 末尾加入实际服务，例如：

```toml
[[services]]
id = "foobar"
alias = "FlowSplice Foobar"
protocol = "tcp"
target = "127.0.0.1:7001"
```

然后只重启已安装服务：

```bash
home_id=$(awk -F'"' '/^id = / { print $2; exit }' \
  "$HOME/Library/Application Support/FlowSplice/Home/homeagent.toml")
launchctl kickstart -k "gui/$(id -u)/io.zxf.flowsplice.homeagent.$home_id"
```

Travel 的映射必须明确指定这个新 Home ID；相同 `service_id` 出现在多个 Home 时，系统不会跨 Home 回退。

## 6. 状态与故障恢复

查看服务：

```bash
home_id=$(awk -F'"' '/^id = / { print $2; exit }' \
  "$HOME/Library/Application Support/FlowSplice/Home/homeagent.toml")
launchctl print "gui/$(id -u)/io.zxf.flowsplice.homeagent.$home_id"
```

如果批准前网络中断，保留原安装目录和原发布包，在包目录重新执行同一条 `init --server <SERVER_IP>`；程序会使用本地 retrieval token 继续等待，不会生成另一个 Home。若证书已经安装但 launchd 启动被中断，重跑同一命令会验证现有文件并补完启动，不会覆盖冲突内容。

Home 的授权防回滚/撤销高水位保存在独立原子状态文件，统计状态保存在 redb；`init --server <IP>` 会在首次安装时自动创建两者。后续正常启动若授权状态丢失或损坏会拒绝运行，不会重建空状态。Travel 另外把历史上验证过的 Relay 保存在自己的 redb 中，并只把它们作为下次启动候选；业务仍必须先拿到当前有效的 Server 签名目录，过期快照不会变成永久授权。

## 验收

- 空白 Mac 只用 `flowsplice-homeagent init --server <SERVER_IP>` 完成身份、证书、TOML、redb 和 launchd 安装。
- 同一 HomeAgent 二进制可配合不同的合法 bootstrap 配置工作；二进制字符串中不包含部署 root、CA、Server 身份或地址。
- 校验码在新 Mac 与批准 Home 上一致。
- 三种权限在页面和运行能力上严格分离。
- Serving-only 没有签发、撤销和批准新 Home 的入口或私钥。
- Home issuer 不能签全局范围，也不能批准第三台 Home。
- Global issuer 可以批准第三台及后续 Home；第一份有效批准幂等生效。
- 错误签发密码不能批准；正确密码批准后新 Home 自动上线。
- Server 停止不会中断已经建立的 Relay 业务流；Relay 故障可切换路径。
