# FlowSplice 0.2 第二 Home macOS Quick Start

这份包用于一台空白的 Apple Silicon Mac。Home 1 与 Home 2 使用同一个 `flowsplice-homeagent`，Home ID 和权限在远程批准时生成，不需要手工准备证书、私钥或 TOML。

## 运行前条件

- 这份包必须由当前部署的根公钥和 Management CA 构建，不能拿其他环境的包加入生产系统。
- 包内还绑定 Server ID、证书名和 control 端口；新 Mac 只输入 IP，但长期 TOML 使用包内证书名完成 TLS 验证。
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

如果批准前网络中断，保留原目录并重新执行同一条 `init --server <SERVER_IP>`；程序会使用本地 retrieval token 继续等待，不会生成另一个 Home。若证书已经安装但 launchd 启动被中断，重跑同一命令会验证现有文件并补完启动，不会覆盖冲突内容。

Home 的授权与统计状态保存在 redb。Travel 另外把历史上验证过的 Relay 保存在自己的 redb 中，并只把它们作为下次启动候选；业务仍必须先拿到当前有效的 Server 签名目录，过期快照不会变成永久授权。

## 验收

- 空白 Mac 只用 `flowsplice-homeagent init --server <SERVER_IP>` 完成身份、证书、TOML、redb 和 launchd 安装。
- 校验码在新 Mac 与批准 Home 上一致。
- 三种权限在页面和运行能力上严格分离。
- Serving-only 没有签发、撤销和批准新 Home 的入口或私钥。
- Home issuer 不能签全局范围，也不能批准第三台 Home。
- Global issuer 可以批准第三台及后续 Home；第一份有效批准幂等生效。
- 错误签发密码不能批准；正确密码批准后新 Home 自动上线。
- Server 停止不会中断已经建立的 Relay 业务流；Relay 故障可切换路径。
