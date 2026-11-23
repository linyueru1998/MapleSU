#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-or-later
"""从 upstream/ + patches/7k/series 生成 applied/（03 分册 §1.3）。

规则（与规格逐条对应）：
  * applied/ 非空且未加 --clean → exit 4（禁止在已叠加的树上再叠补丁）；
  * 逐补丁先 `git apply --check -p1 --whitespace=error-all` 再实际应用，失败 → exit 5
    （**不自动 --3way**，不引入 `patch(1)` 作为主路径）；
  * 应用后把 `uapi/*.h` 同步到两个 symlink 落点（kernel/include/uapi、
    manager/app/src/main/cpp/uapi）——Windows 无 symlink，03 §1.2.4 要求真实目录；
  * 校验：UAPI==7、构建相关范围无 CR（§7.2 G1）、cpp/uapi/ksu.h 可读；
  * 写 applied/.applied.json（不进树哈希）。

退出码：0 ok / 2 缺输入 / 3 upstream 缺失 / 4 重复叠补丁 / 5 补丁失败 /
        6 CR 门禁 / 7 UAPI 校验 / 8 uapi 同步失败。
"""
from __future__ import annotations

import argparse
import re
import sys
from pathlib import Path

import ksu_common as C
from gen_patches import PATCHES

EXIT_OK = 0
EXIT_INPUT = 2
EXIT_UPSTREAM = 3
EXIT_DUPLICATE = 4
EXIT_PATCH = 5
EXIT_CR = 6
EXIT_UAPI = 7
EXIT_SYNC = 8

UAPI_COPIES = ("kernel/include/uapi", "manager/app/src/main/cpp/uapi")


def fail(code: int, msg: str) -> "NoReturn":  # type: ignore[name-defined]
    print(f"ERROR({code}): {msg}", file=sys.stderr)
    raise SystemExit(code)


def git_apply(cwd: Path, patch: Path, check: bool) -> None:
    """逐补丁应用；隔离与「静默 skip 当失败」的判据集中在 ksu_common.git_apply（G5-a）。"""
    C.git_apply(cwd, patch, check=check)


def copy_upstream(upstream: Path, applied: Path) -> None:
    C.copy_tree(upstream, applied)


def sync_uapi_copies(applied: Path) -> list[str]:
    """把 applied/uapi 的每个头文件同步到两个真实目录副本（sandbox for non-symlink)."""
    uapi = applied / "uapi"
    if not uapi.is_dir():
        raise C.KsuError("applied/uapi 缺失")
    headers = sorted(p.name for p in uapi.iterdir() if p.is_file())
    if not headers:
        raise C.KsuError("applied/uapi 为空")
    for rel in UAPI_COPIES:
        dest = applied.joinpath(*rel.split("/"))
        if not dest.is_dir():
            raise C.KsuError(f"uapi 副本目录缺失: {dest}（upstream 快照应已落成真实目录）")
        for name in headers:
            C.write_bytes(dest / name, (uapi / name).read_bytes())
    # 同树断言：三份 supercall.h 必须逐字节相同
    canon = (uapi / "supercall.h").read_bytes()
    for rel in UAPI_COPIES:
        other = applied.joinpath(*rel.split("/")) / "supercall.h"
        if other.read_bytes() != canon:
            raise C.KsuError(f"uapi 同步后仍不一致: {other}")
    return list(UAPI_COPIES)


def validate(applied: Path) -> dict:
    uapi_text = C.read_text(applied / "uapi" / "supercall.h")
    m = re.search(r"KERNEL_SU_UAPI_VERSION\s*=\s*7\s*;", uapi_text)
    if not m:
        raise C.KsuError("applied/uapi/supercall.h 的 KERNEL_SU_UAPI_VERSION 不是 7")
    if "KSU_IOCTL_LOOKUP_SYMBOLS = _IOW('K', 22," not in uapi_text:
        raise C.KsuError("上游 22=LOOKUP_SYMBOLS 被改动（冻结表要求保留）")
    numbers = {int(x) for x in re.findall(r"KSU_IOCTL_\w+\s*=\s*_IO[RW]*\('K',\s*(\d+)", uapi_text)}
    expected = {30, 31, 32, 33, 34, 35, 36}
    if not expected.issubset(numbers):
        raise C.KsuError(f"缺少冻结 ioctl 号 {sorted(expected - numbers)}")
    if 37 in numbers or 38 in numbers or 39 in numbers:
        raise C.KsuError("37-39 是预留号，不得被普通功能占用")
    if len(numbers) != len(set(numbers)):
        raise C.KsuError("ioctl 号重复")
    # 结构体字节稳定性（字段顺序/名字）
    for needle in ("struct ksu_stealth_gen_cmd {", "__u32 manager_gen;", "__u8 _pad[3];",
                   "struct ksu_app_lock_set_cmd {"):
        if needle not in uapi_text:
            raise C.KsuError(f"结构体稳定性断言失败: 缺 {needle!r}")
    bad = C.crlf_offenders(applied)
    if bad:
        raise C.KsuError(f"CR 门禁失败（构建相关范围含 CR）: {bad[:8]}")
    for rel in UAPI_COPIES:
        if not (applied.joinpath(*rel.split("/")) / "ksu.h").is_file():
            raise C.KsuError(f"cpp/kernel uapi 副本不可读: {rel}/ksu.h")
    return {"uapiVersion": 7, "ioctlNumbers": sorted(numbers)}


def build(root: Path, clean: bool, check_only: bool) -> int:
    upstream = root / "upstream"
    applied = root / "applied"
    if not upstream.is_dir():
        fail(EXIT_UPSTREAM, f"缺 upstream/：先跑 scripts/export_snapshot.py --which upstream")
    try:
        series = C.read_series(root / "patches" / "7k" / "series")
    except C.KsuError as exc:
        fail(EXIT_INPUT, str(exc))
    if series != [spec["name"] for spec in PATCHES]:
        fail(EXIT_INPUT, "patch series differs from gen_patches.py; regenerate the patch series")

    if check_only:
        if not applied.is_dir():
            fail(EXIT_INPUT, "applied/ 不存在，--check-only 无意义")
        # 在**仓库外**的临时目录重建，再与现有 applied 比较（不改动 applied）。
        # 不能放 root/.tmp：仓库根被 git init 后那里在 worktree 内，git apply 会静默 skip
        # 全部补丁（G5-a），重建结果退化成 upstream 裸树 → 本项校验反而变成假阴性。
        tmp = C.isolated_apply_root("maplesu-apply-check-")
        try:
            copy_upstream(upstream, tmp)
            _apply_series(tmp, root / "patches" / "7k", series)
            sync_uapi_copies(tmp)
            got = C.tree_sha256(tmp)
            want = C.tree_sha256(applied)
            if got != want:
                fail(EXIT_PATCH, f"applied/ 与重建结果不一致: {want} != {got}"
                                 f"（重跑 apply_patches.py --clean）")
        finally:
            import shutil
            shutil.rmtree(tmp, ignore_errors=True)
        print(f"OK check-only: applied tree matches rebuild ({got})")
        return EXIT_OK

    # 先读出 BUILD-EVIDENCE 再删除（顺序错了会把它一起 rmtree，W12d 实测）
    # applied/BUILD-EVIDENCE.md 是构建证据（生成物，不进树哈希）：--clean 重建时保留它，
    # 避免「重建就把上次的真实产物证据删掉」。
    preserved_evidence = None
    if (applied / "BUILD-EVIDENCE.md").is_file():
        preserved_evidence = (applied / "BUILD-EVIDENCE.md").read_bytes()

    if applied.is_dir() and any(applied.iterdir()):
        if not clean:
            fail(EXIT_DUPLICATE,
                 "applied/ 已存在且非空：拒绝重复叠补丁（主文档:499）。加 --clean 重建。")
        import shutil
        shutil.rmtree(applied)

    print(f"[1/3] copy upstream -> applied ({len(C.iter_tree_files(upstream))} files)")
    copy_upstream(upstream, applied)
    if preserved_evidence is not None:
        C.write_bytes(applied / "BUILD-EVIDENCE.md", preserved_evidence)
    print(f"[2/3] apply {len(series)} patches")
    _apply_series(applied, root / "patches" / "7k", series)
    print("[3/3] sync uapi copies + validate")
    copies = sync_uapi_copies(applied)
    try:
        info = validate(applied)
    except C.KsuError as exc:
        msg = str(exc)
        code = EXIT_CR if "CR" in msg else (EXIT_UAPI if "UAPI" in msg or "ioctl" in msg
                                            or "结构体" in msg else EXIT_SYNC)
        fail(code, msg)

    hashes = C.series_hashes(root / "patches" / "7k")
    _write_notice(applied, hashes)
    C.write_json(applied / ".applied.json", {
        "schema": 1,
        "generator": "third_party/kernelsu/scripts/apply_patches.py@1",
        "order": hashes["order"],
        "aggregateSha256": hashes["aggregateSha256"],
        "seriesSha256": hashes["seriesSha256"],
        "appliedAt": C.utc_now_iso(),
        "uapiVersion": info["uapiVersion"],
        "uapiSyncTargets": copies,
        "treeSha256": C.tree_sha256(applied),
        "fileCount": len(C.iter_tree_files(applied)),
    })
    tree = C.tree_sha256(applied)
    print(f"OK applied: files={len(C.iter_tree_files(applied))} treeSha256={tree} "
          f"uapi={info['uapiVersion']} ioctls={info['ioctlNumbers']}")
    return EXIT_OK


def _apply_series(tree: Path, patch_dir: Path, series: list[str]) -> None:
    for name in series:
        patch = patch_dir / name
        if not patch.is_file():
            raise C.KsuError(f"series 里的补丁不存在: {patch}")
        if b"\r" in patch.read_bytes():
            raise C.KsuError(f"{name}: 补丁含 CR，必须是 LF")
        try:
            git_apply(tree, patch, check=True)
            git_apply(tree, patch, check=False)
        except C.KsuError as exc:
            fail(EXIT_PATCH, str(exc))
        print(f"  + {name}")



def _write_notice(applied: Path, hashes: dict) -> None:
    """applied/NOTICE：来源与许可声明（GPL 材料合规；树哈希排除该文件）。

    审计要求能回答三件事：这份树从哪来（upstream commit + 本仓补丁序列）、
    什么许可（GPL-3.0-or-later + 上游部分 GPL-2.0）、许可证正文在哪（upstream/LICENSE）。
    NOTICE 由脚本生成、每次 rebuild 重写，因此不参与 treeSha256（可复现）。
    """
    upstream_license = applied / "LICENSE"
    notice = f"""MapleSU · third_party/kernelsu/applied
=========================================

本目录是**生成物**，不是手工维护的源码：由
  third_party/kernelsu/scripts/apply_patches.py --clean
从 `third_party/kernelsu/upstream/`（上游 KernelSU，commit
{C.UPSTREAM_COMMIT}）逐字节复制后，按 `patches/7k/series` 顺序应用下列补丁得到：

""" + "".join(f"  - {name}  (sha256 {entry['sha256']})\n"
              for name, entry in zip(hashes["order"], hashes["entries"])) + f"""
补丁序列聚合哈希：{hashes['aggregateSha256']}
补丁序列文件哈希：{hashes['seriesSha256']}
upstream 快照树哈希：见 third_party/kernelsu/manifest.json（upstream.treeSha256）

许可证
------
* 本目录整体按 **GPL-3.0-or-later** 分发（与上游 KernelSU 一致；内核部分另有
  GPL-2.0 条款，见上游 kernel/ 目录文件头 SPDX 标识）。
* 上游许可证正文：`third_party/kernelsu/upstream/LICENSE`（逐字节随快照保留；
  本目录同样带有该文件的副本：`applied/LICENSE`）。
* 7K 移植来源：7kimisu v2.30（commit {C.SOURCE7K_COMMIT}，GPL-3.0），
  其差异以 `patches/7k/*.patch` 形式表达（含来源行号注释）。
* 本目录**不再包含** git 元数据；可复现性与来源证据由
  `third_party/kernelsu/manifest.json`（commit + 逐文件 sha256 + 树哈希 +
  patch series 哈希）提供。

不要手工修改本目录：任何改动都会被
  python third_party/kernelsu/scripts/verify_payload.py
判定为 "applied treeSha256 不符" 并失败。要改就改 upstream/patches 再重建。
"""
    C.write_text(applied / "NOTICE", notice)

def main(argv: list[str]) -> int:
    ap = argparse.ArgumentParser(description="upstream + patches/7k -> applied")
    ap.add_argument("--root", default=str(C.KSU_ROOT))
    ap.add_argument("--clean", action="store_true", help="删除已有 applied/ 后重建")
    ap.add_argument("--check-only", action="store_true", help="只校验 applied/ 是否与重建一致")
    args = ap.parse_args(argv)
    try:
        return build(Path(args.root), args.clean, args.check_only)
    except C.KsuError as exc:
        fail(EXIT_PATCH, str(exc))


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
