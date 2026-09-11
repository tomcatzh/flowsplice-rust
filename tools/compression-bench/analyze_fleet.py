#!/usr/bin/env python3
"""Validate a four-host result set and render speed/CPU comparisons."""

import argparse
import json
import subprocess
import sys
from pathlib import Path

LABELS = ["vps-a", "vps-b", "vps-c", "openwrt"]
NAMES = {"vps-a": "VPS A", "vps-b": "VPS B", "vps-c": "VPS C", "openwrt": "OpenWrt"}
ALGORITHMS = ["zstd-1", "snappy-1", "snappy-2", "lz4-fast"]
DISPLAY = ["Zstd 1", "Snappy 1", "Snappy 2", "LZ4 fast"]
HISTORY = "history-256-ordinary"


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("results", type=Path)
    parser.add_argument("--plots", action="store_true")
    args = parser.parse_args()
    root = args.results
    data = {label: json.loads((root / label / "results.json").read_text()) for label in LABELS}
    hosts = {h["label"]: h for h in json.loads((root / "hosts.json").read_text())}
    verification = json.loads((root / "verification.json").read_text())
    assert {h["label"] for h in verification["hosts"]} == set(LABELS)
    assert all(h["temporary_directory_absent_after_cleanup"] and h["remote_exit_code"] == 0 for h in verification["hosts"])
    baseline = data[LABELS[0]]
    sizes = lambda d: {(c["dataset"], c["algorithm"]["name"]): c["compressed_bytes"] for c in d["cases"]}
    for label, result in data.items():
        assert (root / label / "exit-code.txt").read_text().strip() == "0"
        assert result["schema_version"] == 2
        assert result["rounds"] == 5 and result["sample_ms"] == 120
        assert result["reference_versions"] == baseline["reference_versions"]
        assert result["datasets"] == baseline["datasets"]
        assert sizes(result) == sizes(baseline)
        assert len(result["datasets"]) == 20 and len(result["cases"]) == 200
        assert all(len(c[p + "_samples"]) == 5 for c in result["cases"] for p in ["compression", "decompression"])
        assert hosts[label]["service_observations"]["unchanged"]
        subprocess.run([sys.executable, str(Path(__file__).with_name("analyze.py")), str(root / label)], check=True)

    def case(label, algorithm, dataset=HISTORY):
        return next(c for c in data[label]["cases"] if c["dataset"] == dataset and c["algorithm"]["name"] == algorithm)

    def metric(label, algorithm, phase, key, dataset=HISTORY):
        return case(label, algorithm, dataset)[phase]["median_" + key]

    def matrix(key, digits, dataset=HISTORY):
        lines = ["| 机器 | Zstd 1 | Snappy 1 | Snappy 2 | LZ4 fast |", "|---|---:|---:|---:|---:|"]
        for label in LABELS:
            cells = [f"{metric(label, a, 'compression', key, dataset):,.{digits}f} / {metric(label, a, 'decompression', key, dataset):,.{digits}f}" for a in ALGORITHMS]
            lines.append("| " + " | ".join([NAMES[label], *cells]) + " |")
        return lines

    lines = [
        "# 三台 VPS + OpenWrt：压缩速度与 CPU 实测",
        "",
        "2026-09-11，四台实际设备已完成。每台 20 组内容 × 10 个配置 × 压缩／解压 × 5 轮，四机合计 **8,000 个计时阶段**；800 个设备／内容／算法组合均完成计时前后逐块解压校验。",
        "",
        "**以速度和 CPU 为先，优先考虑 Snappy 1 或 LZ4 fast。** 普通历史页和 4 KiB 终端输出，四台机器上都是 Snappy 1 的压缩中位吞吐更高；96 KiB 历史和真实源码则是 LZ4 fast 的中位吞吐更高。LZ4 fast 通常还明显节省解压 CPU。Zstd 1 是多花 CPU 换更少传输量的候选。Snappy 2 在 OpenWrt 上的压缩成本尤其高，不建议作为通用默认。这里提出候选，尚未把任何算法接入产品。",
        "",
        "## 1. 四组硬件与运行条件",
        "",
        "VPS A／B／C 是固定匿名标签，真实管理地址不放入代码或公开结果。CPU 型号是虚拟机报告值；VPS B 型号名称中的 96-Core 不代表它有 96 个可用核心。",
        "",
        "| 机器 | 报告的 CPU／板型 | 可用逻辑核 | 系统 | 测试秒数 | 测试峰值内存 |",
        "|---|---|---:|---|---:|---:|",
    ]
    short_cpu = {"vps-a": "Xeon Platinum 8269CY", "vps-b": "EPYC 9655P", "vps-c": "EPYC-Milan", "openwrt": "NanoPi R5C · ARM64"}
    for label in LABELS:
        h = hosts[label]
        lines.append(f"| {NAMES[label]} | {short_cpu[label]} | {h['logical_cpu_count']} | {h['os']} | {h['duration_seconds']} | {h['peak_rss_kib']/1024:.1f} MiB |")
    lines += [
        "",
        "官方实现：Zstd **1.5.7**（-3、1、3、6、9），Google Snappy **1.2.2**（1、2），LZ4 **1.10.0**（fast、HC9），另有 memcpy 基线。三台 VPS 运行同一个静态 x86-64 可执行文件，OpenWrt 使用静态 ARM64 文件；性能数据均来自远端实际运行。",
        "",
        "所有机器：单线程、Release、同一份冻结样本、独立块、复用缓冲区；每阶段预热后连续运行至少 120 ms，各阶段随机打乱，共五轮，取每项中位数。低优先级 nice 10，进程地址空间上限 256 MiB；不绑核、不调整系统调频。OpenWrt 保持原有 conservative 调频，上报最高频率约 1.992 GHz。",
        "",
        "峰值内存是整个测试进程的 RSS，包含冻结样本、结果和缓冲区；它不是各算法单独的内存需求比较。",
        "",
        "## 2. 普通历史页：速度",
        "",
        "样本为生成的多字段日志，256 行／页、约 20 KiB，按当前 History JSON 消息体组织，16 页独立压缩。不是从个人 tmux 采集的历史。",
        "",
        "每格为 **压缩 / 解压 MiB/s**，越高越快；两者都按原始字节数计算，1 MiB = 1,048,576 字节。",
        "",
        *matrix("mib_per_second", 1),
        "",
        "## 3. 相同工作量消耗多少 CPU",
        "",
        "连续压测会一直给算法喂数据，所以这些组合的 CPU 占用都接近一个核心的 100%；不能据此说它们一样省 CPU。真正可比的是处理相同原始数据需要多少 CPU 时间。",
        "",
        "下表每格为 **压缩 / 解压 CPU ms/MiB**，越低越省 CPU。",
        "",
        *matrix("cpu_ms_per_mib", 3),
        "",
        "对应的实测占用率如下，每格为 **压缩 / 解压 %**。100% 表示一个逻辑核；在四核 OpenWrt 上约相当于整机计算容量的四分之一。",
        "",
        *matrix("cpu_percent_one_core", 1),
        "",
        "CPU 时间通过进程 user + system 时间测量，占用率 = CPU 时间 / 墙钟时间。各项中位数独立计算，不应要求两个中位数相除严格等于另一个中位数。[getrusage 定义](https://man7.org/linux/man-pages/man2/getrusage.2.html)",
        "",
        "例如 OpenWrt 若持续压缩 10 MiB/s 的这类历史，线性换算的单核 CPU 占用约为：",
        "",
        "| 算法 | 估算单核 CPU 占用 |",
        "|---|---:|",
    ]
    for algorithm, display in zip(ALGORITHMS, DISPLAY):
        value = metric("openwrt", algorithm, "compression", "cpu_ms_per_mib")
        lines.append(f"| {display} | {value:.2f}% |")
    lines += [
        "",
        "这是由实测 CPU ms/MiB 推算的 **固定速率估计**，不是另一次限速运行结果。公式为 `CPU% = CPU ms/MiB × MiB/s ÷ 10`；不包含传输、加密、序列化、调频唤醒和 UI 成本。",
        "",
        "![四机单位数据 CPU 时间](history-cpu.png)",
        "",
        "## 4. Snappy Level 2 的实际代价",
        "",
        "下面是普通历史页中 Level 2 相对于 Level 1 的变化：",
        "",
        "| 机器 | 压缩 CPU 时间倍数 | 解压速度变化 |",
        "|---|---:|---:|",
    ]
    for label in LABELS:
        cost = metric(label, "snappy-2", "compression", "cpu_ms_per_mib") / metric(label, "snappy-1", "compression", "cpu_ms_per_mib")
        speed = (metric(label, "snappy-2", "decompression", "mib_per_second") / metric(label, "snappy-1", "decompression", "mib_per_second") - 1) * 100
        lines.append(f"| {NAMES[label]} | {cost:.2f}× | {speed:+.1f}% |")
    snappy_saving = (1 - case("openwrt", "snappy-2")["compressed_bytes"] / case("openwrt", "snappy-1")["compressed_bytes"]) * 100
    lines += [
        "",
        f"四台机器的压后字节数一致：Level 2 对本组历史仅再减少 **{snappy_saving:.2f}%**。OpenWrt 付出约 2.8 倍压缩 CPU，换来的解压速度差只有几个百分点；小幅速度变化还应考虑波动。真实源码中 Level 2 额外缩小约 8.9%，取舍更合理，但 OpenWrt 的压缩 CPU 仍约为 Level 1 的 2.2 倍。",
        "",
        "测试明确调用官方 Level 2，不是默认 Level 1。官方 1.2.2 头文件仍将 Level 2 标为 experimental；官方通用收益不能替代此处逐内容实测。[Snappy 1.2.0 发布说明](https://github.com/google/snappy/releases/tag/1.2.0)、[1.2.2 接口](https://github.com/google/snappy/blob/1.2.2/snappy.h)",
        "",
        "## 5. 更换内容后，结论会变",
        "",
        "真实 Rust 源码，独立 96 KiB 块；每格仍为 **压缩 / 解压 MiB/s**：",
        "",
        *matrix("mib_per_second", 1, "repo-rust-96k"),
        "",
        "接近 96 KiB 的生成历史页；每格为 **压缩 / 解压 CPU ms/MiB**：",
        "",
        *matrix("cpu_ms_per_mib", 3, "history-256-near96k"),
        "",
        "4 KiB 生成终端原始字节；每格为 **压缩 / 解压 MiB/s**：",
        "",
        *matrix("mib_per_second", 1, "shell-4k"),
        "",
        "这些样本支持保留 Snappy 1 与 LZ4 fast 两个 CPU 优先候选，不能宣布其中一个对所有内容都更快。其余 Markdown、中英文、ANSI 重绘、JSON、短消息、PNG、伪随机内容均保留在每台机器的完整表格中。",
        "",
        "## 6. 高等级压缩：OpenWrt 的发送端成本",
        "",
        "普通历史页，全部配置如下。压后体积只用于解释 CPU 换到了什么；这次重点仍是速度与 CPU。",
        "",
        "| 算法 | 压后占原文 | 压缩 MiB/s | 解压 MiB/s | 压缩 CPU ms/MiB | 解压 CPU ms/MiB |",
        "|---|---:|---:|---:|---:|---:|",
    ]
    for c in data["openwrt"]["cases"]:
        if c["dataset"] != HISTORY:
            continue
        x, y = c["compression"], c["decompression"]
        lines.append(f"| {c['algorithm']['name']} | {c['compressed_percent']:.2f}% | {x['median_mib_per_second']:.1f} | {y['median_mib_per_second']:.1f} | {x['median_cpu_ms_per_mib']:.3f} | {y['median_cpu_ms_per_mib']:.3f} |")
    lines += [
        "",
        "Zstd 6／9、LZ4 HC9 在 OpenWrt 上的压缩 CPU 成本明显高于快速候选。普通历史页中高等级 Zstd 还没有压得比 Level 1 更小。已压缩或伪随机数据的体积收益有限，压完再回退也无法收回已经花掉的 CPU。是否压缩仍应由 PTY 业务根据载荷决定。",
        "",
        "## 7. 可信范围、构建与运行复核",
        "",
        "- 全部四机共 800 组合、8,000 计时阶段完成；每块计时前后均逐字节校验。20 组样本哈希和 200 个压后大小在四台设备及原 Mac 基线间一致。没有采集个人终端内容。",
        "- 编译器 Rust 1.97.1、GCC/G++ 15.2.0；C/C++ 使用 O3。x86 库使用三台 VPS 均已核实支持的 x86-64-v3 基线；ARM64 使用 armv8-a+crc，Snappy NEON/CRC 路径启用。静态 musl/C++ 链接，没有动态加载器或外部共享库依赖。构建容器只用于构建与正确性冒烟，其计时不计入本报告。",
        "- 每种架构 4 项 Release 测试及 200 组合冻结样本冒烟通过。源码、上游归档、原生静态库和最终可执行文件哈希见构建清单。目标设备无需安装编译器或依赖。",
        "- 前后服务观察列表一致（VPS A/B/C 为 31/27/26 个；OpenWrt 为 11 个相关服务进程，不含 SSH 会话子进程）。只执行临时测试，没有修改生产配置、身份、证书、服务或调频设置。所有四机专用临时目录已清理；前后快照不等于持续可用性监测。",
        "- VPS C 的原始 SSH 控制连接最终返回 255，但远端完整结果、退出码 0 和归档已通过另一条连接取回验证；没有把传输连接错误当作成功测试，也没有拿不完整计时补结果。",
        "- 所有吞吐均为预热、内存内、单线程算法性能；不包含 JSON 编解码、网络、TLS、磁盘、冷启动和终端渲染。LZ4 为原始块（原文长度在外），Snappy 为 raw，Zstd 为独立 frame；额外应用封装没有计入。",
        "- Linux 使用低内存 schema 2：每阶段在计时外创建并预热一个上下文。原 Mac 基线为 schema 1，保留原始记录；没有用新数据覆盖它，也不把两次方法差异当作硬件结论。",
        "- 非独占系统，以下相对 MAD 是五轮速度的描述性波动，不是置信区间。几个百分点的速度差不能作为强结论；CPU 占用不等于功耗。",
        "",
        "| 机器 | 400 项速度相对 MAD：中位数 | 95 分位 | 全程 CPU steal |",
        "|---|---:|---:|---:|",
    ]
    for label in LABELS:
        h = hosts[label]
        mad = h["speed_relative_mad_percent"]
        lines.append(f"| {NAMES[label]} | {mad['median']:.2f}% | {mad['p95']:.2f}% | {h['global_cpu_steal_percent']:.3f}% |")
    lines += [
        "",
        "iOS／Android 继续只考虑解压。官方解码库和 Rust 调用路径的交叉编译／链接已经验证，但本次 OpenWrt ARM64 结果不代表 iPad 或 Android 手机性能；没有将编译通过写成真机性能通过。详情见 [移动端构建边界](../../CROSS_COMPILE.md)。",
        "",
        "## 8. 选择建议与完整数据",
        "",
        "| 你的优先目标 | 候选 | 本次四机证据 |",
        "|---|---|---|",
        "| Home 端处理常见终端输出和普通历史尽量轻 | **Snappy 1** | 这两类样本在四机上压缩最快；OpenWrt 普通历史仅 3.81 CPU ms/MiB |",
        "| 更低解压成本，同时兼顾复杂大块内容 | **LZ4 fast** | VPS 解压优势大；96 KiB 历史和源码压缩也胜 Snappy 1；OpenWrt 普通历史解压与 Snappy 接近 |",
        "| 愿意多花 CPU 减少历史传输量 | **Zstd 1** | OpenWrt 普通历史压缩 10.47 CPU ms/MiB；体积为原文 11.07%，Snappy 1 为 24.32% |",
        "| 固定使用 Snappy，愿意加重编码换体积 | Snappy 2 | 源码有收益，普通历史收益小；OpenWrt 成本偏高，应按内容选择 |",
        "",
        "现在若只按速度和 CPU 决策，我会把 **Snappy 1、LZ4 fast 放在第一组**，Zstd 1 留作流量优先选项。最终算法、阈值和协议仍由你选择；通用 FlowSplice 加密传输层没有引入自动压缩策略。",
        "",
        "- 四组完整数据：" + "；".join(f"[{NAMES[label]} 的全部 200 项]({label}/measurements.zh-CN.md)" for label in LABELS) + "。",
        "- 每组目录均含 `results.json`（所有轮次、块延迟与哈希）、`summary.csv`、`run.log`、`host.json` 和退出码。",
        "- [四机硬件与负载](hosts.json)、[x86-64 构建清单](amd64-build.json)、[ARM64 构建清单](arm64-build.json)、[复核与清理收据](verification.json)。",
        "- [Rust 基准程序](../../src/main.rs)、[冻结样本读写](../../src/frozen.rs)、[内容来源](../../CORPUS.md)、[运行方法](../../README.md)、[静态 Linux 构建方法](../../BUILD_LINUX.md)。",
        "- [原 Mac 报告](../2026-09-11-m4-max/REPORT.zh-CN.md) 保留为历史基线；此次四机结果补足其未测 Linux 的边界。",
        "",
    ]
    (root / "REPORT.zh-CN.md").write_text("\n".join(lines))

    if args.plots:
        import matplotlib
        matplotlib.use("Agg")
        import matplotlib.pyplot as plt

        colors = ["#6470AC", "#BD772A", "#E8AF64", "#318982"]
        for key, title, filename, fmt in [
            ("cpu_ms_per_mib", "CPU ms per original MiB (lower is better)", "history-cpu.png", ".2f"),
            ("mib_per_second", "Original MiB/s (higher is better)", "history-speed.png", ",.0f"),
        ]:
            fig, axes = plt.subplots(2, 4, figsize=(16, 7.4))
            for row, phase in enumerate(["compression", "decompression"]):
                for col, label in enumerate(LABELS):
                    ax = axes[row, col]
                    values = [metric(label, a, phase, key) for a in ALGORITHMS]
                    ax.barh(DISPLAY, values, color=colors, height=.65)
                    ax.invert_yaxis()
                    ax.set_xlim(0, max(values) * 1.33)
                    ax.set_title(f"{NAMES[label]} · {phase.capitalize()}", fontsize=11, loc="left", pad=12)
                    ax.grid(axis="x", alpha=.18)
                    ax.set_axisbelow(True)
                    ax.spines[["top", "right", "left"]].set_visible(False)
                    ax.tick_params(axis="y", length=0)
                    for i, value in enumerate(values):
                        ax.text(value + max(values)*.025, i, format(value, fmt), va="center", fontsize=10)
            fig.suptitle(title, x=.02, y=.98, ha="left", fontsize=18, fontweight="bold")
            fig.text(.02, .915, "Synthetic PTY history: 256 rows / ~20 KiB page · Five-round medians on each actual host", fontsize=11, color="#444444")
            fig.text(.02, .02, "Independent axis scales · Warm single-thread codec benchmark · No network/TLS/UI time · 2026-09-11", fontsize=10, color="#444444")
            fig.tight_layout(rect=(0, .065, 1, .9), h_pad=3, w_pad=2)
            fig.savefig(root / filename, dpi=160)
            plt.close(fig)
    print("Validated four hosts and wrote report plus 800-case measurement tables.")


if __name__ == "__main__":
    main()
