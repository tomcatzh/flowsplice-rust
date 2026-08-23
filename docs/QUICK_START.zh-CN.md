# FlowSplice 0.2 Travel 快速开始

## 准备

使用 0.2 Travel 公开包。`flowsplice-travelagent` 是与具体部署无关的通用二进制；公开包不含任何真实部署配置、根公钥、签名 deployment trust、Management CA、IP 或子域名。

公开包只允许包含：

```text
flowsplice-travel-0.2.0-macos-arm64/
├── bin/flowsplice-travelagent
├── travel-bootstrap.example.toml
├── QUICK_START.zh-CN.md
└── SHA256SUMS
```

`travel-bootstrap.example.toml` 只是使用 IANA 保留示例域名的结构样例，不能直接用于部署。通过私密渠道取得本部署的根公钥、签名 trust 和真实 Relay 地址后，在公开仓库之外复制并填写为 `travel-bootstrap.toml`。真实 IP 和任何子域名都按密钥级信息处理；主域公开不代表其子域可公开。

macOS arm64 文件位于发布包的 `macos-arm64/flowsplice-travelagent`。免费签名是 ad-hoc codesign：它能校验文件未被签名后修改，但不提供 Apple Developer ID 身份，也没有 notarization。若文件经浏览器下载而被 Gatekeeper 隔离，仍可能需要在 macOS 的“隐私与安全性”页面由用户明确允许。

ad-hoc 签名和包内 `SHA256SUMS` 不能证明发布者身份。首次使用前，必须通过另一个可信渠道核对整个包的 SHA-256；私下取得部署根公钥后，还应通过独立可信渠道核对其 SHA-256 指纹。

解包后进入包目录，验证全部文件和 ad-hoc 签名：

```bash
shasum -a 256 -c SHA256SUMS
chmod 755 ./bin/flowsplice-travelagent
codesign --verify --strict --verbose=2 ./bin/flowsplice-travelagent
```

随后在该目录私下放置 `travel-bootstrap.toml`、`deployment-root.pub` 和
`deployment-trust.json`。绝不能把填写后的文件复制回仓库、公共构建目录或公开 Release。

## 第一次远程注册：开始时不需要 TOML 和 cert 目录

Travel 只提供 `enroll-remote` 这一条身份注册路径，不提供申请文件导出或签发结果文件导入命令。

选择一个全新的 Travel ID 和一个空安装目录。首次注册只需要 Travel ID、要申请的 Home ID 和安装目录：

```bash
mkdir -m 700 ./my-travel
./bin/flowsplice-travelagent enroll-remote \
  --travel-id travel-laptop \
  --home-id home-1 \
  --install-dir ./my-travel
```

命令会要求输入并再次确认一个至少 12 个字符的 Travel 私钥密码。然后它会：

1. 在 Travel 本机生成两把独立、加密保存的 Management/Business 私钥；
2. 读取 `travel-bootstrap.toml`，验证根签名 trust，并依次尝试配置中的 Relay 建立首次注册 TLS 通道；
3. 把只有公钥和 proof-of-possession 的 enrollment 请求送到指定 Home；
4. 在终端显示 `Home verification code`；
5. 保持运行并重试，等待 Home 上的人工批准。

私钥不会上传。此时命令等待远程返回是正常状态，但不会自动批准或签发。

## 在 Home 本地批准

保持 Travel 机器上的 `enroll-remote` 命令运行。它会停在等待状态，并继续通过 Relay 轮询签发结果。

走到 Home 所在的另一台机器，在那台 Home 机器的浏览器中打开本地签发页面：

```text
http://127.0.0.1:9081
```

这个 HTTP 页面保持 loopback-only，只在 Home 机器本地操作。Travel 与 Home 可以位于不同网络；无需远程打开 Home 页面，无需 SSH 隧道，也无需人工传递 enrollment 文件。

在 Home 页面中：

1. 打开收到的远程 Travel 请求；
2. 对比页面与 Travel 终端显示的 verification code；
3. 选择最小必要授权范围：指定业务、当前 Home 或确有必要时的全局授权；
4. 选择有效期；
5. 在正式审批对话框中输入 Home 签发密码，点击“批准并远程返回”。

Home 签发密码只在 Home 本机解锁签发密钥，不会发送给 Server、Relay 或 Travel。错误密码不会生成凭据。

当前 Home 只有在部署信任配置了独立全局授权密钥时才显示“全局超级授权”。普通第二 Home 只能批准自己的 Home 或自己的指定业务；serving-only Home 不显示批准、签发或撤销入口。

## 自动安装结果

Home 批准后，签发结果通过 `Home -> Server -> Relay -> Travel` 返回。等待中的 `enroll-remote` 会自动解除等待，验证部署信任、请求/响应绑定、双证书链、Travel 身份、两把公钥、授权范围和有效期，然后创建：

```text
my-travel/
├── travelagent.toml
├── cert/
│   ├── deployment-root.pub
│   ├── travel-management.crt
│   ├── travel-management.key
│   ├── travel-business.crt
│   ├── travel-business.key
│   ├── management-ca.crt
│   ├── business-ca.crt
│   ├── deployment-trust.json
│   └── enrollment-response.json
└── state/
    └── travel-state.redb
```

`travelagent.toml` 不保存 Travel 私钥密码。正常流程不需要下载、复制或上传 `enrollment-request.json` / `enrollment-response.json`。

如果从较早的 0.2 开发包替换二进制，现有 Travel 证书和私钥无需重发，但运行 TOML 必须显式包含 `deployment_trust = ".../deployment-trust.json"`。不要让程序根据证书目录或固定文件名猜测 trust 路径。0.1.1 与 0.2 的协议升级仍按整体升级边界处理。

## 启动 Travel

```bash
./bin/flowsplice-travelagent --config ./my-travel/travelagent.toml
```

输入刚才设置的 Travel 私钥密码。随后：

- `http://127.0.0.1:9080` 是 Travel 本地页面；
- 页面可查看当前业务、Relay、五分钟统计和日/周/月/年报表；
- 启动后 Travel 会向 Home 确认新凭据已经真正启用，双方随后清理 enrollment 生命周期记录。

## TOML 什么时候需要修改

第一次远程注册已经生成身份、根公钥路径、信任、Home、Relay seed 和本地状态所需的完整 TOML。首次注册不要求业务映射；获得凭据并启动 Travel 后，在本地 Web 页面按实际需要创建业务监听。

通常只修改以下内容：

- 增加 `[[homes]]` 后使 Travel 重新读取可访问的 Home；
- 调整本地 `ui_listen` 或容量/超时参数；
- 在确有 bootstrap 可用性需要时增加 `[[seed_relays]]`。

不要把 Home SPKI、完整 Relay 授权名单或密码手工写进 TOML。`[[seed_relays]]` 只是首次取得签名目录的联系地址。Travel 会把历史上从有效签名目录中验证过的 Relay 长期保存在 `travel-state.redb`，以后重启时把它们也当成 bootstrap 候选；旧记录永远不能代替新的 Server 签名目录授权。

首次 enrollment 具体连接哪个 Relay，完全由 `travel-bootstrap.toml` 的 `bootstrap_relays` 决定。程序会规范化、排序、去重后逐一轮询；一个 Relay 失败会继续尝试下一个。Relay 地址变化只修改并重新分发配置文件，不重新编译二进制。

业务映射不再写入 TOML。在 `http://127.0.0.1:9080` 的“Service mappings”区域选择 Home 与业务，填写本地监听地址（例如 `127.0.0.1:10080`）并创建。修改已有监听的地址或端口后点击 Apply，会先确认新端口能够绑定，再原子写入 `travel-state.redb` 并切换监听；无需重启。若新端口无效或已占用，旧监听和持久状态保持不变。Remove 会停止该本地监听并删除其持久记录。

从较早的 0.2 开发版本升级时，程序会在第一次启动时把旧 TOML 中的 `[[mappings]]` 一次性导入 redb。此后 redb 优先，继续修改旧 TOML 不会改变运行映射；确认 Web 中记录无误后可删除旧条目。

## 换发和撤销

已注册 Travel 可在 `http://127.0.0.1:9080` 发起远程换发。Home 仍需在本地页面人工选择范围、点击批准并输入 Home 签发密码。响应返回后，Travel 端输入 Travel 私钥密码安装；重启 Travel 后新身份生效并向 Home 回执。

删除/撤销凭据必须在签发它的 Home 页面执行，并再次输入 Home 签发密码。错误密码不改变状态。成功操作产生不可回滚的签名撤销记录；界面隐藏活动凭据不代表删除审计与防回滚历史。

Home 后台只提供远程审批，不再提供上传申请文件、下载签发结果的手工入口。首次注册和换发都通过现有控制连接自动传输公开申请与签名响应。
