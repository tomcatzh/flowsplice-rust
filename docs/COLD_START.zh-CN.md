# FlowSplice 0.2 全系统 Cold Start

本文用于从零建立一套新的 FlowSplice 0.2 部署：离线部署根、两套 CA、Server、至少一个 Relay、首个 Global Home、首个 Travel，以及后续 Home 的单命令加入。它是公开文档，所有名称、域名、地址和目录均为占位示例；不要把生产密码、私钥、真实节点清单或临时工作目录提交到仓库。

这不是 0.1.1 的滚动升级指南。0.2 使用协议 v2，Server 不再有业务数据端口，也不允许 0.1.1/0.2.0 数据面混跑。

## 1. 最终拓扑与开放端口

先写下本部署自己的值，后文统一替换：

| 名称 | 本文示例 | 说明 |
| --- | --- | --- |
| Deployment ID | `example-prod-v1` | 一经投产不随意改变 |
| Server ID | `server-1` | TLS URI 与控制签名均绑定此 ID |
| Server DNS/IP | `server.example.net` / `192.0.2.10` | Home 主动连接 |
| 首个 Home ID | `home-1` | 初始 Global issuer |
| Relay ID | `relay-1` | 每个 Relay 必须唯一 |
| Relay DNS/IP | `relay-1.example.net` / `198.51.100.20` | Travel 可访问 |

公网防火墙只需要：

| 节点 | TCP 端口 | 用途 |
| --- | --- | --- |
| Server | `7443` | Home 控制连接与新 Home bootstrap |
| Relay | `8443` | Server/Travel 管理连接与 Travel bootstrap |
| Relay | `8444` | Travel/Home 直接业务 Carrier；Relay 只转发端到端密文 |

`9080` 至 `9084` 的页面全部只监听 `127.0.0.1`。Server 不得监听旧的 `7444` 或任何业务数据端口。新 Home 不需要公网入站端口。

## 2. 秘密与公开材料分区

在一台受控的初始化工作站创建三个互不混用的目录。以下路径仅为示例：

```bash
umask 077
mkdir -p ./flowsplice-cold-start/{offline-root,issuer-private,public,nodes,state}
chmod 700 ./flowsplice-cold-start/{offline-root,issuer-private,nodes,state}
chmod 755 ./flowsplice-cold-start/public
```

| 位置 | 内容 | 是否可进入发布包 |
| --- | --- | --- |
| `offline-root/` | 加密 deployment-root 私钥 | 永远不可以；签完 trust 后离线保存 |
| `issuer-private/` | 两套 CA 私钥、Travel authority 私钥、Home enrollment authority 私钥 | 不可以；只安装到明确批准的 issuer Home |
| `nodes/` | Server/Relay/首个 Home 的叶私钥 | 不可以；分别只交付到对应节点 |
| `public/` | 根公钥、CA 公共证书、签名 deployment trust、公开签名公钥 | 可以 |
| `state/` | Server 初始授权/代次文件 | 不可以放入通用发布包；只交付 Server |

私钥文件保持 `0600`，私有目录保持 `0700`。不要把密码写进 TOML、命令行参数、shell history、Docker build argument 或仓库。Cold Start 完成后应把离线根私钥和恢复材料转移到独立加密介质，并验证恢复副本可读。

## 3. 构建离线工具

在可信源码 checkout 中构建根信任工具：

```bash
cargo build --locked --release -p flowsplice-enrollment --bin flowsplice-trust
```

普通 Docker 构建、测试和发布必须复用本地镜像与缓存：

```bash
export FLOWSPLICE_DOCKER_PULL=false
```

只有明确安排基础镜像更新时才把它改成 `true`。Docker 的缓存规则是上游层改变才会使后续层失效，详见 [Docker build cache](https://docs.docker.com/build/cache/)。

## 4. 创建离线部署根

这条命令交互输入并确认至少 12 个字符的独立根密码：

```bash
./target/release/flowsplice-trust root-init \
  --output-dir ./flowsplice-cold-start/offline-root
```

结果：

```text
offline-root/deployment-root.key   # 加密私钥，离线
offline-root/deployment-root.pub   # 公开 P-256 公钥
```

部署根只签 deployment trust，不应被复制到 Server、Relay、Home 或 Travel。运行节点只取得 `deployment-root.pub`。

## 5. 创建两套 CA 与运行时签名密钥

FlowSplice 把 Management TLS 与端到端 Business TLS 分成两套 CA。下面命令均会交互询问私钥密码；对首个 Global Home 所持有的两把 CA 私钥和三把 authority 私钥，输入同一份 Home 签发密码，因为 Home 的一次批准操作需要同时解锁它们。

```bash
cd ./flowsplice-cold-start

openssl genpkey -algorithm EC -pkeyopt ec_paramgen_curve:P-256 \
  -aes-256-cbc -out issuer-private/management-ca.key
openssl req -x509 -new -sha256 -days 1825 \
  -key issuer-private/management-ca.key \
  -subj '/CN=FlowSplice Management CA' \
  -addext 'basicConstraints=critical,CA:TRUE' \
  -addext 'keyUsage=critical,keyCertSign,cRLSign' \
  -out public/management-ca.crt

openssl genpkey -algorithm EC -pkeyopt ec_paramgen_curve:P-256 \
  -aes-256-cbc -out issuer-private/business-ca.key
openssl req -x509 -new -sha256 -days 1825 \
  -key issuer-private/business-ca.key \
  -subj '/CN=FlowSplice Business CA' \
  -addext 'basicConstraints=critical,CA:TRUE' \
  -addext 'keyUsage=critical,keyCertSign,cRLSign' \
  -out public/business-ca.crt

# 三次都输入同一份 Home 签发密码。后两次会先用 FlowSplice 自己的
# 高成本私钥解密逻辑验证已有 key，避免 OpenSSL 默认内存上限造成误判。
../target/release/flowsplice-trust authority-init \
  --output-dir issuer-private/home-1-authority
../target/release/flowsplice-trust authority-init \
  --output-dir issuer-private/global-authority \
  --verify-key issuer-private/home-1-authority/issuer-authority.key
../target/release/flowsplice-trust authority-init \
  --output-dir issuer-private/home-enrollment-authority \
  --verify-key issuer-private/home-1-authority/issuer-authority.key \
  --verify-key issuer-private/global-authority/issuer-authority.key

openssl genpkey -algorithm EC -pkeyopt ec_paramgen_curve:P-256 \
  -out nodes/server-control.key
```

authority 和 Server control 公钥在 deployment trust 中使用 65 字节未压缩 P-256 点的十六进制形式。可用 OpenSSL 导出 DER 后提取最后 65 字节，并立即检查首字节为 `04`、总长度为 130 个十六进制字符：

```bash
public_hex() {
  openssl pkey -in "$1" -pubout -outform DER \
    | tail -c 65 \
    | xxd -p -c 65
}

cp issuer-private/home-1-authority/issuer-authority.pub \
  public/home-1-authority.pub
cp issuer-private/global-authority/issuer-authority.pub \
  public/global-authority.pub
cp issuer-private/home-enrollment-authority/issuer-authority.pub \
  public/home-enrollment-authority.pub
public_hex nodes/server-control.key > public/server-control.pub

for key in public/*-authority.pub public/server-control.pub; do
  test "$(tr -d '\r\n' < "$key" | wc -c | tr -d ' ')" = 130
  grep -Eq '^04[0-9a-fA-F]{128}$' "$key"
done
```

不要用 `openssl pkey` 代替 `flowsplice-trust authority-init` 去验证 FlowSplice 加密的 issuer key。后者使用与运行时完全相同的 scrypt 参数和内存策略；密码也不要通过命令行参数传入。

## 6. 为固定节点签发叶证书

每张叶证书必须恰好包含一个 FlowSplice URI 身份：

```text
flowsplice://identity/<role>/<id>
```

使用下面的辅助函数。`dns` 是证书中的 DNS SAN，不能用来替代 URI 身份；`eku` 必须使用表中的值。

```bash
issue_leaf() {
  name="$1" role="$2" stable_id="$3" dns="$4" eku="$5" ca="$6"

  openssl req -new -newkey ec -pkeyopt ec_paramgen_curve:P-256 \
    -sha256 -nodes -subj "/CN=$name" \
    -keyout "nodes/$name.key" -out "nodes/$name.csr"

  {
    echo 'basicConstraints=critical,CA:FALSE'
    echo 'keyUsage=critical,digitalSignature'
    echo "extendedKeyUsage=$eku"
    echo "subjectAltName=URI:flowsplice://identity/$role/$stable_id,DNS:$dns"
  } > "nodes/$name.ext"

  openssl x509 -req -sha256 -days 825 \
    -in "nodes/$name.csr" \
    -CA "public/$ca.crt" -CAkey "issuer-private/$ca.key" \
    -CAcreateserial -extfile "nodes/$name.ext" \
    -out "nodes/$name.crt"

  openssl verify -CAfile "public/$ca.crt" "nodes/$name.crt"
  openssl x509 -in "nodes/$name.crt" -noout -ext subjectAltName \
    | grep -F "URI:flowsplice://identity/$role/$stable_id"
}
```

首套部署需要：

```bash
issue_leaf server server server-1 server.example.net \
  serverAuth,clientAuth management-ca
issue_leaf relay-1 relay relay-1 relay-1.example.net \
  serverAuth management-ca
issue_leaf home-1-management home home-1 home-1-management.example.net \
  clientAuth management-ca
issue_leaf home-1-business home home-1 home-1.example.net \
  serverAuth business-ca
```

删除 CSR、扩展临时文件和 CA serial 临时文件前，先验证每张证书。Travel 和后续 Home 不在这里预签叶证书：它们在自己的机器生成私钥和 CSR，经人工批准后取得证书。

## 7. 生成并签署 deployment trust

先计算首个 Home 的两把 SPKI pin：

```bash
certificate_pin() {
  openssl x509 -in "$1" -pubkey -noout \
    | openssl pkey -pubin -outform DER \
    | openssl dgst -sha256 -r \
    | awk '{print $1}'
}

certificate_pin nodes/home-1-management.crt
certificate_pin nodes/home-1-business.crt
```

创建 `public/deployment-trust-payload.json`。CA 字段必须嵌入两个公共 PEM 的原文；下面仅展示结构，不能把 `<...>` 原样保留：

```json
{
  "version": 1,
  "deployment_id": "example-prod-v1",
  "generation": 1,
  "not_before_unix_secs": 0,
  "not_after_unix_secs": 0,
  "management_ca_certificate_pem": "-----BEGIN CERTIFICATE-----\n...\n-----END CERTIFICATE-----\n",
  "business_ca_certificate_pem": "-----BEGIN CERTIFICATE-----\n...\n-----END CERTIFICATE-----\n",
  "server_control_keys": [
    {
      "server_id": "server-1",
      "epoch": 1,
      "public_key": "04..."
    }
  ],
  "home_endpoints": [
    {
      "home_id": "home-1",
      "management_spki_pins": ["...64 hex characters..."],
      "business_spki_pins": ["...64 hex characters..."]
    }
  ],
  "home_enrollment_authorities": [
    {
      "id": "operator-home-enrollment-authority",
      "epoch": 1,
      "issuer_home_id": "home-1",
      "public_key": "04..."
    }
  ],
  "travel_authorities": [
    {
      "kind": "home",
      "id": "home-1-authority",
      "epoch": 1,
      "home_id": "home-1",
      "public_key": "04..."
    },
    {
      "kind": "global",
      "id": "operator-global-authority",
      "epoch": 1,
      "home_id": "home-1",
      "public_key": "04..."
    }
  ]
}
```

时间使用当前 Unix 时间减 300 秒作为 `not_before_unix_secs`，并选择早于 CA 到期日的 `not_after_unix_secs`。确认 generation、ID、所有公钥、SPKI 和 CA 原文后，在能读取离线根的机器上签名：

```bash
./target/release/flowsplice-trust sign \
  --payload ./flowsplice-cold-start/public/deployment-trust-payload.json \
  --root-key ./flowsplice-cold-start/offline-root/deployment-root.key \
  --output ./flowsplice-cold-start/public/deployment-trust.json
```

`sign` 拒绝覆盖已有结果。续期或变更 trust 时必须提高 generation，写入新文件，验证后再原子替换。使用同一根公钥签署更高 generation 只需替换独立的 trust 文件。更换 deployment root 必须走明确的 current/next 根迁移与带外指纹验证，但仍不得重新编译 Home/Travel 二进制。

## 8. 初始化 Server 持久状态

Server 故意拒绝凭空猜测初始授权状态。创建两个 owner-only 文件：

```bash
cat > state/server-authorization.json <<'JSON'
{
  "version": 1,
  "snapshot": {
    "generation": 1,
    "home_endpoint_credentials": [],
    "credentials": [],
    "revocations": []
  },
  "used_enrollment_requests": []
}
JSON

cat > state/server-control-generation.json <<'JSON'
{"next_generation":1}
JSON

chmod 600 state/server-authorization.json state/server-control-generation.json
```

这两个文件和 `server-state.redb` 是生产状态，必须持续备份；不能在 Server 重启时重新生成。Server 的五分钟全局报表只收集节点签名汇总并幂等去重，不从控制指令或业务中转字节推断业务量。

## 9. 配置 Server、Relay 与首个 Global Home

以仓库中的三个 example TOML 为基准复制，不要使用 `tests/e2e` 配置：

- [Server example](../server/config.example.toml)
- [Relay example](../relay/config.example.toml)
- [Home example](../homeagent/config.example.toml)

### Server 必填关系

- `control_listen = "0.0.0.0:7443"`；不存在 `data_listen`。
- `cert`/`key` 使用 Server Management 叶证书。
- `deployment_root_public_key` 和 `deployment_trust` 使用已验证的公共材料。
- `control_signing_key` 与 trust 中 `server_control_keys` 的公钥一致。
- `travel_authorization_state`、`control_generation_state` 和 `state_store` 使用持久目录。
- `[[homes]]` 至少列出根信任中的 `home-1`。
- 每个 `[[relays]]` 的 `id`、`management_addr`、`data_public_addr` 与对应 Relay 完全一致。
- `ui_listen` 保持 loopback。

### Relay 必填关系

- `management_listen = "0.0.0.0:8443"`，`data_listen = "0.0.0.0:8444"`。
- `data_public_addr` 是 Travel 与 Home 真正可达的公网地址。
- Relay 叶证书 URI 必须是 `flowsplice://identity/relay/<relay-id>`。
- `server_spki_pins` 是 Server 叶证书的 SHA-256 SPKI pin。
- `travel_authorization_cache` 和 `state_store` 各 Relay 独立，不共享文件。
- `ui_listen` 保持 loopback。

### 首个 Global Home 必填关系

- Management/Business 证书分别使用两套 CA，URI 都绑定 `home-1`。
- `server_control_addr` 指向 Server 的 `7443`，`server_spki_pins` 固定 Server 叶证书。
- `[issuer]` 安装两把加密 CA 私钥。
- `[issuer.home_authority]`、`[issuer.global_authority]`、`[issuer.home_enrollment_authority]` 分别安装三把加密 authority 私钥，ID 与 trust 完全一致。
- 所有 issuer 私钥使用同一 Home 签发密码。
- `services` 只发布本机真实存在的业务；不要把示例 SSH 服务照抄进生产。
- `ui_listen` 保持 loopback。批准 Travel、撤销凭据、批准新 Home 都必须在 Home 本机页面点击并输入签发密码。

先做无副作用检查：

```bash
flowsplice-server --config /etc/flowsplice/server.toml --check-config
flowsplice-relay --config /etc/flowsplice/relay.toml --check-config
```

配置和公共信任确认无误后，对手工配置的首个 Home 与每个 Relay 各执行一次显式授权状态初始化：

```bash
flowsplice-homeagent initialize-authorization-state --config /etc/flowsplice/homeagent.toml
flowsplice-relay --config /etc/flowsplice/relay.toml --initialize-authorization-state
```

命令幂等，但只会创建缺失的空状态，绝不覆盖已有状态。正常启动遇到授权状态丢失或损坏时必须 fail-closed；不要用初始化命令代替恢复备份。通过 `init --server <IP>` 加入的后续 Home 会在首次安装事务中自动完成初始化，无需增加第二条命令。

Home 没有单独的 `--check-config`；首次以前台启动观察一次，确认配置、证书、trust、SPKI 和 issuer key 绑定均通过，再交给服务管理器。

## 10. 启动顺序与服务管理

推荐顺序：Server → Relay → 首个 Global Home。全部进程应以专用非 root 用户运行，配置/私钥只有该用户可读，日志不得包含密码、私钥、route/work secret 或业务 payload。

Linux 可用 systemd，macOS 用户进程使用 launchd。Apple 推荐用 launchd 管理 per-user background agent，并从用户的 `Library/LaunchAgents` 装载，参见 [Creating Launch Daemons and Agents](https://developer.apple.com/library/archive/documentation/MacOSX/Conceptual/BPSystemStartup/Chapters/CreatingLaunchdJobs.html)。

至少设置：程序绝对路径、`--config` 绝对路径、自动重启、受控日志路径和 `RUST_LOG=info`。启动后验证：

```bash
ss -lntp
curl --fail http://127.0.0.1:9083/
curl --fail http://127.0.0.1:9084/
curl --fail http://127.0.0.1:9081/
```

macOS 用 `lsof -nP -iTCP -sTCP:LISTEN` 代替 `ss`。确认 Server 只有控制端口和 loopback UI；Relay 有 management/data 与 loopback UI；Home 只有 loopback UI 和主动出站连接。

## 11. 构建与部署无关的正式程序和独立配置包

正式 Home/Travel/Server/Relay 可执行文件不得包含 deployment root、CA、节点 ID、IP、域名、端口或 Relay 列表。所有部署值都必须位于独立配置文件；修改配置不得重新编译程序。

先构建一次通用程序：

```bash
export FLOWSPLICE_DOCKER_PULL=false
./scripts/build-release.sh
```

再分别准备 Home 和 Travel 的发布配置目录。两个目录都包含 `deployment-root.pub` 和同一份根签名 `deployment-trust.json`，但 bootstrap TOML 各自保存角色需要的拓扑：

```text
package-config/home/
├── home-bootstrap.toml
├── deployment-root.pub
└── deployment-trust.json

package-config/travel/
├── travel-bootstrap.toml
├── deployment-root.pub
└── deployment-trust.json
```

`home-bootstrap.toml`：

```toml
deployment_root_public_key = "deployment-root.pub"
deployment_trust = "deployment-trust.json"
server_id = "server-1"
server_name = "server.example.net"
server_control_port = 7443
ui_listen = "127.0.0.1:9082"
```

`travel-bootstrap.toml`：

```toml
deployment_root_public_key = "deployment-root.pub"
deployment_trust = "deployment-trust.json"
bootstrap_relays = ["relay-1.example.net:8443"]
ui_listen = "127.0.0.1:9080"
```

构建配置化 macOS 包：

```bash
export FLOWSPLICE_HOME_BOOTSTRAP_CONFIG_FILE="$PWD/package-config/home/home-bootstrap.toml"
export FLOWSPLICE_TRAVEL_BOOTSTRAP_CONFIG_FILE="$PWD/package-config/travel/travel-bootstrap.toml"
./scripts/build-home2-macos-package.sh
./scripts/build-travel-macos-package.sh
```

仓库的 Dockerfile 还必须把 frontend 和所有基础镜像锁到不可变 digest。`FLOWSPLICE_DOCKER_PULL=false` 禁止主动更新；如果本机缺少该 digest，应停止并安排一次明确的缓存准备，不能在普通发布或部署时临时下载新层。

macOS 文件使用免费的 ad-hoc codesign 与稳定的 `io.zxf.flowsplice.*` identifier。它能校验签名后未被修改，但没有 Apple Developer ID 身份，也未 notarize；公开分发时必须如实说明 Gatekeeper 限制。

记录每个发布文件的 SHA-256，并从解包后的最终文件重新验证 checksum、架构和 codesign。客户端包应包含公开 bootstrap TOML、root 公钥和签名 trust，但不得包含 `*.key`、密码、运行期 endpoint TOML、证书、凭据或 token。用 `strings` 检查二进制不得出现配置中的 root、公网 Relay、Server ID/域名或 CA PEM。

## 12. 首个 Travel 从空目录注册

Travel 机器不预先创建运行期 TOML 或 cert；从包含 `travel-bootstrap.toml` 的包目录执行：

```bash
mkdir -m 700 ./my-travel
./flowsplice-travelagent enroll-remote \
  --travel-id travel-laptop \
  --home-id home-1 \
  --install-dir ./my-travel
```

Travel 本机输入并确认自己的私钥密码，终端随后保持等待。在首个 Global Home 本机打开 `http://127.0.0.1:9081`，核对校验码、选择最小授权范围与有效期、点击批准并输入 Home 签发密码。无需 SSH、远程打开 Home 页面或人工传 enrollment 文件。

批准后 Travel 自动产生 cert、TOML 和 `travel-state.redb`。启动并输入 Travel 私钥密码，再在 Travel 本地 Web 页面创建所需业务监听；监听立即生效并保存在 redb，不修改 TOML、不重启。完整流程见 [Travel Quick Start](QUICK_START.zh-CN.md)。

## 13. Home 2、Home 3 及后续 Home

使用包含独立 `home-bootstrap.toml` 和 trust 文件的 Home macOS 包。在空 Apple Silicon Mac 的包目录只运行：

```bash
./bin/flowsplice-homeagent init --server 192.0.2.10
```

命令生成本机私钥和 CSR，显示 Home ID/校验码，然后等待。任意在线 Global Home 可在自己的 loopback 页面批准，并选择 Serving-only、Home issuer 或 Global issuer。第一份通过密码、签名和请求摘要验证的批准生效，迟到结果幂等忽略；普通 Home issuer 和 Serving-only Home 无权批准新 Home。

批准后程序自动安装证书、公开信任、endpoint credential、权限对应的加密 issuer bundle、TOML、redb、固定二进制和 launchd。程序不会猜测业务，初始 `services` 为空。完整流程见 [Second Home macOS Quick Start](HOME2_QUICK_START.zh-CN.md)。

## 14. Cold Start 验收

只有下列证据全部通过，才算环境建立完成：

1. Server、每个 Relay、首个 Home 的配置和证书身份检查通过；手工配置的 Home/Relay 已显式初始化授权状态，删除该状态后的启动负向测试必须失败。
2. Server 无业务 listener；业务路径为 `Travel ↔ Relay ↔ Home`。
3. Travel 从真正空目录远程注册，Home 错误签发密码被拒绝，正确密码后自动生成证书/TOML/redb。
4. Home 2 从真正空环境只执行一次 `init --server <IP>`，批准后三种权限与文件权限符合预期；重跑同一命令可幂等恢复安装。
5. 同一个 Home/Travel 二进制分别使用两套不同的测试 bootstrap 配置，不重新构建即可选择不同拓扑；二进制字符串中不存在任一配置的 root、CA、节点身份、地址、域名或端口。
6. Travel 通过映射访问 Home 的真实 TCP 业务，并核对请求/响应内容，而不只检查端口连通。
7. 至少两个 Relay 时，停止当前 Relay 后同一 TCP Flow 切换并继续；不是新建连接掩盖失败。
8. 停止 Server 后，已经建立的业务 Flow 继续；新路由可以失败，但不得存在隐藏的 Server data fallback。
9. 恢复 Server/Relay 后控制目录、Catalog 与授权 generation 单调恢复。
10. Travel 重启从 redb 得到历史 Relay 启动候选；Server 离线且没有新鲜签名目录时，本地业务连接必须失败，目录恢复后才可承运。
11. Travel、Relay、Home 的统计只在各自第二页点击后加载；Server 只接收签名五分钟汇总并幂等去重，日/周/月/年窗口可查询。
12. 撤销 Travel 凭据必须再次输入签发密码；在线与重启后均拒绝已撤销凭据。
13. 所有日志与发布归档通过秘密扫描，未出现密码、私钥、route/work secret、真实恢复 token 或业务 payload。

Docker 验收使用同一功能边界，但只能使用 `tests/e2e/generated/` 的一次性测试 PKI；测试凭据绝不能进入真实环境。

## 15. 备份、恢复与变更

首次启动前和每次替换二进制/配置/trust 前，为每个节点创建带时间戳的可恢复备份，并保留：

- Server：配置、叶证书/私钥、control key、authorization JSON、generation JSON、redb；
- Relay：配置、叶证书/私钥、authorization cache、redb；
- Home：配置、双叶身份、issuer 私钥、签发 ledger、authorization cache、redb；
- 离线：deployment root 私钥/公钥、两套 CA、authority 私钥、签名 trust、发布 SHA-256 清单。

恢复时必须整组恢复同一时间点的身份、trust 与持久状态，不能只把 generation 文件清零。先停止服务、保留故障现场、恢复到新目录验证，再原子切换并观察日志。不要以删除 redb、authorization state 或 rollback high-water mark 的方式“修复”签名/代次错误。

变更 deployment trust 时：

1. 保留 deployment ID 和根公钥；
2. 提高 generation 与需要轮换的 key epoch；
3. 设置重叠有效期；
4. 离线签署新文件；
5. 先在隔离副本运行配置验证，再备份并分发；
6. 确认所有节点接受更高 generation 后才归档旧文件。

本次部署配置边界修正不重新签发现有 Home/Travel 证书，也不更换 deployment root 或 CA。Home 运行配置已经显式引用 root/trust，可直接替换二进制；较早 0.2 Travel 运行配置在替换二进制前必须补充显式 `deployment_trust` 路径。首次 Home/Travel 包始终携带独立 bootstrap TOML、公共 root 和根签名 trust。不得为兼容旧开发包恢复任何编译期部署默认值。

0.1 授权 JSON 没有 `home_endpoint_credentials` 字段。0.2 Server 第一次正常启动读取这种旧格式时，会在开放 listener 之前自动把 Travel authorization generation 提高一次、补齐字段并原子写回；`--check-config` 仍然只读。滚动升级必须先升级能读取新字段的 Relay、Home 与 Travel，再启动新 Server。不要手工只改 JSON 数字；先对生产状态副本执行迁移演练，并以 `travel_authorization_schema_migrated` 和所有节点对新 generation 的 ack 作为验收证据。

更换根公钥、CA 或稳定身份属于重新建立信任，不是普通续期。应准备并演练独立迁移方案，不能声称对现有 0.2 身份无缝兼容。

## 16. 禁止事项

- 不把旧签名快照或历史 Relay 记录提升为永久授权。
- 不给 Server 增加业务 data listener、socket pairing 或业务字节统计。
- 不在 Server/Relay/Travel 保存 Home 签发密码。
- 不通过公网暴露 Home/Travel/Relay/Server 的统计或批准页面。
- 不给 Serving-only 或普通 Home issuer 隐式增加 Global 能力。
- 不从 `tests/e2e` 复制测试密码、测试证书或 `allow_unencrypted_test_keys` 到生产。
- 不在普通构建或部署中反复拉取 Docker image。
- 不把 ad-hoc codesign 描述为 Developer ID 签名或 notarization。
