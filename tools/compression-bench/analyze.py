#!/usr/bin/env python3
"""Create complete measurement tables and optional matplotlib comparison charts."""
import argparse
import json
from pathlib import Path

def main():
    p = argparse.ArgumentParser()
    p.add_argument("results", type=Path)
    p.add_argument("--plots", action="store_true")
    a = p.parse_args()
    data = json.loads((a.results / "results.json").read_text())
    assert data["finished_unix_seconds"] > data["started_unix_seconds"]
    assert all(len(c["compression_samples"]) == data["rounds"] and len(c["decompression_samples"]) == data["rounds"] for c in data["cases"])
    lines = ["# 全部压缩基准数据", "", "所有速度均按原始 MiB/s 计算；CPU 100% 表示占满一个核心。`压／解` 表示压缩／解压。耗时是每原始 MiB 的进程 CPU 毫秒。数据为各轮中位数；各轮原始值、范围、块哈希见 results.json。大小仅包含库输出，未加应用与 TLS 封装。", ""]
    for ds in data["datasets"]:
        lines += [f"## {ds['name']}", "", f"{ds['block_count']} 块，共 {ds['original_bytes']:,} 字节；每块 {ds['min_block_bytes']:,}–{ds['max_block_bytes']:,} 字节。", "", ds["description"], "",
                  "| 算法 | 压后占原文 | 原文÷压后 | 压 MiB/s | 解 MiB/s | CPU% 压／解 | CPU ms/MiB 压／解 | p95 μs/块 压／解 |",
                  "|---|---:|---:|---:|---:|---:|---:|---:|"]
        for c in [c for c in data["cases"] if c["dataset"] == ds["name"]]:
            x, y = c["compression"], c["decompression"]
            lines.append(f"| {c['algorithm']['name']} | {c['compressed_percent']:.2f}% | {c['ratio_original_over_compressed']:.2f}× | {x['median_mib_per_second']:.1f} | {y['median_mib_per_second']:.1f} | {x['median_cpu_percent_one_core']:.1f} / {y['median_cpu_percent_one_core']:.1f} | {x['median_cpu_ms_per_mib']:.3f} / {y['median_cpu_ms_per_mib']:.3f} | {c['compression_latency']['p95_us']:.2f} / {c['decompression_latency']['p95_us']:.2f} |")
        lines += [""]
    (a.results / "measurements.zh-CN.md").write_text("\n".join(lines) + "\n")
    if a.plots:
        import matplotlib
        matplotlib.use("Agg")
        import matplotlib.pyplot as plt
        from matplotlib.ticker import MaxNLocator
        for ds_name, title, filename in [
            ("history-256-ordinary", "Synthetic PTY history: 256 log rows per page", "history-comparison.png"),
            ("repo-rust-96k", "Genuine repository Rust source: independent 96 KiB blocks", "source-comparison.png"),
        ]:
            cases = [c for c in data["cases"] if c["dataset"] == ds_name and c["algorithm"]["name"] != "none-copy"]
            names = [c["algorithm"]["name"] for c in cases]
            colors = ["#6267BC" if n.startswith("zstd") else "#D68532" if n.startswith("snappy") else "#288E88" for n in names]
            fig, axes = plt.subplots(1, 3, figsize=(15, 5.5), sharey=True)
            values = [[c["compressed_percent"] for c in cases],
                      [c["compression"]["median_cpu_ms_per_mib"] for c in cases],
                      [c["decompression"]["median_cpu_ms_per_mib"] for c in cases]]
            for ax, vals, label in zip(axes, values, ["Compressed size (% of original)", "Compression CPU (ms / MiB)", "Decompression CPU (ms / MiB)"]):
                ax.barh(names, vals, color=colors, height=.65)
                ax.set_xlabel(label + "\nLower is better", fontsize=10)
                ax.grid(axis="x", alpha=.18)
                ax.set_axisbelow(True)
                ax.spines[["top", "right", "left"]].set_visible(False)
                ax.tick_params(axis="y", length=0)
                ax.xaxis.set_major_locator(MaxNLocator(5))
                ax.set_xlim(0, max(vals) * 1.25)
                for i, value in enumerate(vals):
                    ax.text(value + max(vals)*.018, i, f"{value:.2f}", va="center", fontsize=9)
            axes[0].invert_yaxis()
            fig.suptitle(title, x=.02, ha="left", fontsize=15, fontweight="bold")
            fig.text(.02, .025, "Apple M4 Max / macOS 26.6.2  |  Five shuffled rounds  |  Independent warm blocks  |  Mobile performance not measured", fontsize=9, color="#555555")
            fig.tight_layout(rect=(0, .07, 1, .92))
            fig.savefig(a.results / filename, dpi=160)
            plt.close(fig)

if __name__ == "__main__":
    main()
