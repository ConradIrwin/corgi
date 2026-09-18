#!/usr/bin/env python3
"""Package the pinned Xcode Metal tools without installing or selecting Xcode.

Like build-libclang.sh, reconcile a release before doing expensive packaging.
Uploads use the same Corgi release repository, with RELEASE_REPO as an override.
The default is a local dry run. XCODE_APP supplies the exact source installation;
obtaining that installation and accepting Apple's terms are operator actions.
"""

import hashlib
import json
import os
import plistlib
import re
import shutil
import subprocess
import sys
import tarfile
import tempfile
from pathlib import Path


ROOT = Path(__file__).resolve().parent.parent


def run(*arguments, **kwargs):
    try:
        return subprocess.run(arguments, check=True, **kwargs)
    except subprocess.CalledProcessError as error:
        if error.stderr:
            print(error.stderr, file=sys.stderr, end="")
        raise


def pinned_xcode_build():
    matches = re.findall(
        r'^pub const METAL_XCODE_BUILD: &str = "([A-Za-z0-9]+)";$',
        (ROOT / "src/main.rs").read_text(), re.MULTILINE,
    )
    if len(matches) != 1:
        raise ValueError("expected one METAL_XCODE_BUILD constant in src/main.rs")
    return matches[0]


def find_xcode(build):
    """Find the exact build without consulting or changing xcode-select."""
    override = os.environ.get("XCODE_APP")
    candidates = [Path(override)] if override else sorted(Path("/Applications").glob("Xcode*.app"))
    for candidate in candidates:
        with (candidate / "Contents/version.plist").open("rb") as source:
            version = plistlib.load(source)
        if version["ProductBuildVersion"] == build:
            return candidate.resolve(), version["CFBundleShortVersionString"]
    raise ValueError(f"Xcode build {build} not found; set XCODE_APP to that exact installation")


def validate_links(stage):
    """Reject links that require the source Xcode or escape the shipped tree."""
    root = stage.resolve()
    for path in stage.rglob("*"):
        if path.is_symlink():
            if Path(os.readlink(path)).is_absolute():
                raise ValueError(f"absolute symlink in Metal bundle: {path}")
            target = path.resolve(strict=True)
            if not target.is_relative_to(root):
                raise ValueError(f"symlink escapes Metal bundle: {path}")


def copy_toolchain(source, stage):
    """Expose the real tools, not Apple's installed-toolchain lookup launchers."""
    payload = source / "usr/metal/current"
    (stage / "bin").mkdir(parents=True)
    for tool in ("metal", "air-lld"):
        shutil.copy2(payload / "bin" / tool, stage / "bin" / tool)
    (stage / "bin/metallib").symlink_to("air-lld")
    shutil.copytree(payload / "lib/clang", stage / "lib/clang", symlinks=True)
    validate_links(stage)


def verify(stage, xcode, provenance, work):
    """Compile and link with the original developer installation inaccessible."""
    profile = work / "verify.sb"
    # sandbox-exec parameters avoid interpolating paths into Scheme source.
    profile.write_text(
        '(version 1)\n(allow default)\n'
        '(deny file-read* (subpath (param "XCODE")))\n'
        '(deny file-read* (subpath "/Applications") (subpath "/Library/Developer") '
        '(subpath "/Volumes") '
        '(subpath "/private/var/run/com.apple.security.cryptexd") '
        '(subpath "/private/var/run/cryptexd") '
        '(subpath "/System/Library/AssetsV2/com_apple_MobileAsset_MetalToolchain"))\n'
        '(deny file-read* '
        '(subpath "/System/Library/PrivateFrameworks/GPUCompiler.framework"))\n'
        '(deny process-exec (literal "/usr/bin/xcrun") '
        '(literal "/usr/bin/xcodebuild") (literal "/usr/bin/xcode-select"))\n'
    )
    prefix = [
        "/usr/bin/sandbox-exec", "-D", f"XCODE={xcode.resolve()}", "-f", str(profile)
    ]
    environment = {
        "PATH": f"{stage / 'bin'}:/usr/bin:/bin",
        "HOME": str(work),
        "TMPDIR": str(work),
        "LANG": "C",
    }
    output = run(
        *prefix, str(stage / "bin/metal"), "--version",
        env=environment, capture_output=True, text=True,
    ).stdout
    expected = f"Apple metal version {provenance['metal_version']} "
    if not any(line.startswith(expected) for line in output.splitlines()):
        raise ValueError(f"expected {expected.strip()}, got:\n{output}")
    print(output, end="")

    header = work / "scene.h"
    header.write_text("typedef struct { unsigned int increment; } Scene;\n")
    shader = work / "probe.metal"
    shader.write_text(
        '#include <metal_stdlib>\nusing namespace metal;\n'
        'kernel void increment(device uint *values [[buffer(0)]], '
        'constant Scene &scene [[buffer(1)]], uint index [[thread_position_in_grid]]) '
        '{ values[index] += min(scene.increment, 42u); }\n'
    )
    air = work / "probe.air"
    library = work / "probe.metallib"
    # GPUI uses these flags and force-includes its generated C scene definitions.
    run(*prefix, str(stage / "bin/metal"), "-gline-tables-only",
        "-mmacosx-version-min=10.15.7", "-MO", "-c", str(shader),
        "-include", str(header), "-o", str(air), env=environment)
    run(*prefix, str(stage / "bin/metallib"), str(air), "-o", str(library),
        env=environment)
    with library.open("rb") as source:
        if source.read(4) != b"MTLB":
            raise ValueError("Metal verification did not produce a metallib")


def archive_metadata(entry):
    """Machine ownership and packaging time must not change the artifact bytes."""
    entry.uid = entry.gid = entry.mtime = 0
    entry.uname = entry.gname = ""
    entry.pax_headers = {}
    return entry


def existing_release(repository, tag, asset):
    # Listing distinguishes a missing tag from authentication/network failures.
    # Direct tag lookup would otherwise make every gh failure look like a miss.
    releases = json.loads(run(
        "gh", "api", "--paginate", "--slurp", f"repos/{repository}/releases",
        capture_output=True, text=True,
    ).stdout)
    for page in releases:
        for release in page:
            if release["tag_name"] == tag:
                assets = {entry["name"]: entry for entry in release["assets"]}
                required = (asset, f"{asset}.sha256")
                if release["draft"] or any(
                    name not in assets or assets[name]["state"] != "uploaded"
                    or assets[name]["size"] <= 0 for name in required
                ):
                    raise ValueError("Metal release is incomplete; inspect it before retrying")
                checksum = run(
                    "gh", "api", assets[f"{asset}.sha256"]["url"],
                    "-H", "Accept: application/octet-stream",
                    capture_output=True, text=True,
                ).stdout.split()
                if (len(checksum) != 2 or checksum[1] != asset
                        or len(checksum[0]) != 64
                        or any(c not in "0123456789abcdef" for c in checksum[0])
                        or assets[asset].get("digest") != f"sha256:{checksum[0]}"):
                    raise ValueError("Metal release checksum does not match its asset digest")
                return checksum[0]
    return None


def export_release_sha256(digest):
    """Hand the pinned digest to release builds of Corgi, which embed it."""
    github_env = os.environ.get("GITHUB_ENV")
    if github_env:
        with open(github_env, "a") as env_file:
            env_file.write(f"CORGI_METAL_SHA256={digest}\n")


def main():
    build = pinned_xcode_build()
    tag, asset = f"metal-{build}", "metal-macos.tar.zst"
    publish = os.environ.get("DRY_RUN", "1") == "0"
    repository = os.environ.get("RELEASE_REPO", "ConradIrwin/corgi")
    if publish:
        if not repository:
            raise ValueError("publishing requires RELEASE_REPO and authenticated gh")
        published = existing_release(repository, tag, asset)
        if published is not None:
            print(f"{repository}: {tag}/{asset} already exists")
            # Export the pin even on the no-op path: release builds run after
            # this step and embed the digest whether or not it was rebuilt.
            export_release_sha256(published)
            return

    if sys.platform != "darwin":
        raise ValueError("Metal packaging and verification require macOS")
    xcode, version = find_xcode(build)
    toolchain = xcode / "Contents/Developer/Toolchains/XcodeDefault.xctoolchain"
    output = run(
        str(toolchain / "usr/metal/current/bin/metal"), "--version",
        capture_output=True, text=True,
    ).stdout
    match = re.search(r"^Apple metal version ([0-9.]+) ", output, re.MULTILINE)
    if not match:
        raise ValueError(f"cannot read Metal version from Xcode build {build}:\n{output}")
    provenance = {
        "xcode_version": version,
        "xcode_build": build,
        "metal_version": match[1],
    }
    if not shutil.which("zstd"):
        raise ValueError("install zstd before packaging Metal")
    destination = ROOT / "dist" / tag
    if destination.exists():
        raise ValueError(f"refusing to overwrite {destination}; move it before retrying")
    destination.parent.mkdir(exist_ok=True)

    with tempfile.TemporaryDirectory(prefix="metal-", dir=destination.parent) as temp:
        work = Path(temp)
        stage = work / "stage"
        copy_toolchain(toolchain, stage)
        # Keep the originating Apple notices with the extracted components.
        notices = xcode / "Contents/Resources"
        (stage / "licenses").mkdir()
        for name in ("LicenseInfo.plist", "en.lproj/License.rtf"):
            shutil.copy2(notices / name, stage / "licenses" / Path(name).name)
        (stage / "CORGI_METAL.json").write_text(json.dumps(provenance, indent=2) + "\n")
        archive = work / asset
        environment = dict(os.environ, COPYFILE_DISABLE="1")
        tar = work / "metal.tar"
        print(f"Packaging Metal from {xcode}", flush=True)
        with tarfile.open(tar, "w") as output:
            output.add(stage, arcname=".", filter=archive_metadata)
        run("zstd", "-q", "-T0", "-19", str(tar), "-o", str(archive))

        # Verify the shipped bytes after a round trip, not the staging directory.
        unpacked = work / "unpacked"
        unpacked.mkdir()
        run("tar", "-xf", str(archive), "-C", str(unpacked), env=environment)
        validate_links(unpacked)
        verify(unpacked, xcode, provenance, work)
        with archive.open("rb") as source:
            digest = hashlib.file_digest(source, "sha256").hexdigest()
        # Release builds of Corgi embed this to hard-pin the Metal download.
        export_release_sha256(digest)
        output = work / "output"
        output.mkdir()
        shutil.move(archive, output / asset)
        (output / f"{asset}.sha256").write_text(f"{digest}  {asset}\n")
        output.rename(destination)

    print(f"Verified Metal archive: {destination / asset}")
    if publish:
        # No --clobber: racing publishers or packaging fixes must not replace a pin.
        run("gh", "release", "create", tag, str(destination / asset),
            str(destination / f"{asset}.sha256"), "--repo", repository, "--draft",
            "--latest=false",
            "--title", f"Metal from Xcode {version} ({build})",
            "--notes", f"Metal {provenance['metal_version']}. Packaged by ci/build-metal.py.")
        run("gh", "release", "edit", tag, "--repo", repository,
            "--draft=false", "--latest=false")


if __name__ == "__main__":
    try:
        main()
    except (OSError, ValueError, subprocess.CalledProcessError) as error:
        sys.exit(f"error: {error}")
