# Termius 官方界面研究

检索与视觉核对日期：2026-09-08。参考官方文章、官方文章中的界面图和开发者 App Store 描述；没有安装或操作当前 Termius 客户端，因此不把文章发布时间等同于当前构建验证。

## 值得借鉴的工作方式

1. **管理数据和正在工作的终端分开。** 2024 年桌面改版把连接资料集中到 Vaults，并用水平标签承载终端；文章明确解释了旧侧栏把数据、连接和文件传输混在一起导致复杂度上升的问题。对应本项目，应让 Home 管理、会话目录和工作终端各自拥有完整页面。[官方桌面导航说明](https://termius.com/blog/termius-x)
2. **横向标签和纵向上下文各有取舍。** 2025 年 Workspaces 文章说明，打开大量终端时，单靠横向标签和命令面板仍不够；将相关终端组织在同一工作区，并允许专注于单个终端，有助于保留上下文。这里研究其组织方法，不顺带引入多 pane 或命令广播。[Workspaces 设计说明](https://termius.com/blog/workspaces)
3. **把终端作为工作主体。** 2026 年 Focus 说明展示一个主要终端配旁边的其他终端列表；重点是随时知道还有哪些工作在进行。部分命令/目录信息依赖其额外能力，不能从图片推断 FlowSplice 已经具备。[Focus 官方说明及截图](https://termius.com/blog/workspaces-focus-without-losing-context)
4. **手机与平板共享概念，但不机械缩小桌面。** iOS 改版文章给出手机底部导航和平板顶部标签。官方配图已在浏览器中直接查看。FlowSplice 的手机终端应单独占屏，系统键盘出现时隐藏无关导航；平板可以保留更多同时打开的终端上下文。[iOS 导航说明](https://termius.com/blog/termius-for-ios-new-navigation-and-sftp)
5. **Android 为形态适配保留差异。** 官方说明将手机底部导航、平板顶部标签以及折叠屏转换分开设计。文章中的部分后续 SFTP 计划不是当时已经完成的功能。[Android 导航说明](https://www.termius.com/blog/termius-for-android-a-final-milestone-in-termius-redesign)
6. **移动输入值得单独设计。** 2026 年官方指引讨论可配置按键条、组合/粘贴输入及触控方向键。这里只把它们当作后续输入体验评估参考，不增加未讨论的 snippet/history 功能。[移动输入指引](https://termius.com/blog/8-tips-for-using-ai-agents-on-mobile-in-termius)

## 实际查看的官方图

- [桌面专注终端](https://framerusercontent.com/images/EnTxX3ehgXwbH05E6bUNho4sU.jpg)
- [iPhone 与 iPad 导航](https://framerusercontent.com/images/3aNBoEXOY0Z7HNzNWW6WVoPnIo8.png?height=661&width=1600)

## 迁移到 FlowSplice 的结论

用户新补充的多 Home 与命名会话，让页面层次应当成为 Home → 远端会话 → 本机已打开终端。多 Home 的授权状态、连接状态与某个 session 的读写角色不可混为一个全局状态。借鉴 Termius 的实用组织与终端空间分配，保留 FlowSplice 的加密、授权、单写入者与 tmux 生命周期规则。

没有证据表明参考文章提供了与 FlowSplice 完全相同的单写入者抢夺协议，因此接管流程源于本项目已确认需求，不归因于 Termius。
