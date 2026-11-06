"""根据锁定依赖生成发行包中的第三方许可证文本"""

import argparse
import json
from pathlib import Path
import subprocess


def main():
    """读取 Cargo 元数据并收集依赖随包提供的许可证

    @param: 可选的目标平台，默认 Linux x86_64
    @return: 无，缺少许可证时停止打包
    """
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--target", default="x86_64-unknown-linux-gnu")
    args = parser.parse_args()
    root = Path(__file__).resolve().parents[1]
    result = subprocess.run(
        ["cargo", "metadata", "--locked", "--offline", "--format-version", "1",
         "--filter-platform", args.target, "--manifest-path", str(root / "Cargo.toml")],
        check=True, capture_output=True, text=True,
    )
    metadata = json.loads(result.stdout)
    parts = ["第三方依赖许可证\n依赖版本以 Cargo.lock 为准；包含构建与测试依赖的许可证\n"]
    for package in sorted(metadata["packages"], key=lambda p: (p["name"], p["version"])):
        if package["name"] == "cpr-session-failover":
            continue
        source = Path(package["manifest_path"]).parent
        files = set()
        if package.get("license_file"):
            files.add(source / package["license_file"])
        for pattern in ["LICENSE*", "LICENCE*", "COPYING*", "NOTICE*"]:
            files.update(f for f in source.glob(pattern) if f.is_file())
        # 公开 SDK 在原仓库根目录共享 Apache-2.0 许可证
        if not files and package["name"] == "gateway-plugin-sdk":
            files.add(source.parents[3] / "LICENSE")
        if not files or not all(f.is_file() for f in files):
            raise RuntimeError(f"依赖缺少随包许可证：{package['name']} {package['version']}")
        parts.append(f"\n{'=' * 72}\n依赖：{package['name']} {package['version']}\n"
                     f"许可证：{package.get('license', '未声明')}\n"
                     f"来源：{package.get('repository') or package.get('source') or 'Cargo 包'}\n")
        for path in sorted(files):
            parts.append(f"\n--- {path.name} ---\n{path.read_text(errors='strict').rstrip()}\n")
    output = root / "THIRD_PARTY_LICENSES.txt"
    output.write_text("".join(parts))
    print(f"已生成：{output.name}")


if __name__ == "__main__":
    main()
