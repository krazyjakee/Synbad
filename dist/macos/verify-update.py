"""Check that both legacy and current updaters receive a complete Mac app."""

import pathlib
import sys
import tarfile


def verify(directory, version, target):
    expected = f"synbad-{version}-{target}.tar.gz"
    # Mirrors the pre-0.1.13 updater. GitHub currently lists assets by name;
    # checking every match also keeps compatibility independent of ordering.
    matches = sorted(
        path.name
        for path in directory.iterdir()
        if target in path.name and path.name.endswith((".tar.gz", ".tgz"))
    )
    if matches != [expected]:
        raise ValueError(f"legacy updater could select an incomplete archive: {matches}")

    arch = target.removesuffix("-apple-darwin")
    core = directory / f"deskflow-core-{version}-macos-{arch}.tar.gz"
    with tarfile.open(core) as archive:
        names = set(archive.getnames())
        for name in ("deskflow-client", "deskflow-server", "DESKFLOW-LICENSE", "DESKFLOW-LICENSE-EXCEPTION"):
            if name not in names:
                raise ValueError(f"Core download is missing {name}")

    root = expected.removesuffix(".tar.gz")
    with tarfile.open(directory / expected) as archive:
        files = {entry.name for entry in archive if entry.isfile()}
    app = f"{root}/Synbad.app/Contents"
    required = [
        f"{app}/Info.plist",
        f"{app}/_CodeSignature/CodeResources",
        f"{app}/Resources/synbad.icns",
        f"{app}/Resources/DESKFLOW-LICENSE",
        f"{app}/Resources/DESKFLOW-LICENSE-EXCEPTION",
    ]
    for name in ("synbad-gui", "synbadd", "deskflow-client", "deskflow-server"):
        # Older updaters need the flat binaries; newer ones replace the app.
        required.extend((f"{root}/{name}", f"{app}/MacOS/{name}"))
    missing = set(required) - files
    if missing:
        raise ValueError(f"Synbad update archive is incomplete: {sorted(missing)}")
    print(f"Verified legacy asset selection, complete signed app and Core download: {target}")


if __name__ == "__main__":
    verify(pathlib.Path(sys.argv[1]), sys.argv[2], sys.argv[3])
