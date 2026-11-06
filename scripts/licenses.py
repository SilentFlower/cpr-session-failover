"""根据锁定依赖生成发行包中的第三方许可证文本"""

import argparse
import json
from pathlib import Path
import subprocess


def read_notice(path):
    """保留上游许可文本内容，统一去除行末空白

    @param path 上游许可证或许可说明文件
    @return 可直接合并的许可文本
    """
    return "\n".join(line.rstrip() for line in path.read_text(errors="strict").splitlines()).rstrip()


def append_frontend_licenses(parts, root):
    """收集锁文件解析出的前端运行依赖许可证

    @param parts 合并后的许可证文本片段
    @param root 插件工程目录
    @return 无，缺少随包许可证时停止打包
    """
    result = subprocess.run(
        ["pnpm", "--dir", str(root / "frontend"), "list", "--prod", "--depth", "Infinity", "--json"],
        check=True, capture_output=True, text=True,
    )
    pending = list(json.loads(result.stdout)[0]["dependencies"].values())
    packages = {}
    while pending:
        item = pending.pop()
        source = Path(item["path"])
        package = json.loads((source / "package.json").read_text())
        identity = (package["name"], package["version"])
        if identity in packages:
            continue
        packages[identity] = (source, package)
        # pnpm 同时列出构建 peer 与其他平台的可选包，这些不属于页面运行依赖
        required = package.get("dependencies", {})
        optional = package.get("optionalDependencies", {})
        for name, dependency in item.get("dependencies", {}).items():
            if name in required or (name in optional and Path(dependency["path"]).is_dir()):
                pending.append(dependency)
    for (name, version), (source, package) in sorted(packages.items()):
        files = sorted(path for path in source.iterdir() if path.is_file()
                       and path.name.upper().startswith(("LICENSE", "LICENCE", "COPYING", "NOTICE")))
        # GSAP 等 npm 包只在 README 中提供许可说明，保留上游原文及其许可链接
        if not files and package.get("license"):
            files = sorted(path for path in source.iterdir() if path.is_file()
                           and path.name.lower().startswith("readme")
                           and "license" in path.read_text(errors="strict").lower())
        if not files:
            raise RuntimeError(f"前端依赖缺少随包许可证：{name} {version}")
        repository = package.get("repository", "npm 包")
        if isinstance(repository, dict):
            repository = repository.get("url", "npm 包")
        parts.append(f"\n{'=' * 72}\n前端依赖：{name} {version}\n"
                     f"许可证：{package.get('license', '未声明')}\n来源：{repository}\n")
        for path in files:
            parts.append(f"\n--- {path.name} ---\n{read_notice(path)}\n")


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
    parts = ["第三方依赖许可证\nRust 依赖版本以 Cargo.lock 为准，包含构建与测试依赖\n"
             "前端运行依赖以 frontend/pnpm-lock.yaml 为准\n"]
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
            parts.append(f"\n--- {path.name} ---\n{read_notice(path)}\n")
    append_frontend_licenses(parts, root)
    output = root / "THIRD_PARTY_LICENSES.txt"
    output.write_text("".join(parts))
    print(f"已生成：{output.name}")


if __name__ == "__main__":
    main()
