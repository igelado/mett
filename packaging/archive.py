#!/usr/bin/env python3
"""Archives DIR as OUT: a .zip, or else a .tar.gz, holding DIR itself.

Reproducible: entries are sorted, dated SOURCE_DATE_EPOCH, owned by root,
and their modes are 755 for executables (`mett` and `mett-gui`, with
`.exe` on Windows) and 644 for the rest, wherever the archive is made.

Usage: archive.py OUT DIR
"""

import gzip
import io
import os
import sys
import tarfile
import time
import zipfile
from pathlib import Path


def main() -> None:
    out, root = Path(sys.argv[1]), Path(sys.argv[2])
    epoch = int(os.environ.get("SOURCE_DATE_EPOCH", "0"))
    files = sorted(p for p in root.rglob("*") if p.is_file())
    entry = lambda p: f"{root.name}/{p.relative_to(root).as_posix()}"
    executables = ("mett", "mett.exe", "mett-gui", "mett-gui.exe")
    mode = lambda p: 0o755 if p.name in executables else 0o644
    if out.suffix == ".zip":
        # Zip dates have no zone (UTC here), and start in 1980.
        date = time.gmtime(max(epoch, 315532800))[:6]
        with zipfile.ZipFile(out, "w", zipfile.ZIP_DEFLATED) as zip:
            for path in files:
                info = zipfile.ZipInfo(entry(path), date)
                info.external_attr = (0o100000 | mode(path)) << 16
                info.compress_type = zipfile.ZIP_DEFLATED
                zip.writestr(info, path.read_bytes())
        return
    data = io.BytesIO()
    with tarfile.open(fileobj=data, mode="w", format=tarfile.PAX_FORMAT) as tar:
        for path in files:
            info = tarfile.TarInfo(entry(path))
            info.size = path.stat().st_size
            info.mtime = epoch
            info.mode = mode(path)
            info.uname = info.gname = "root"
            with path.open("rb") as f:
                tar.addfile(info, f)
    with out.open("wb") as f, gzip.GzipFile(fileobj=f, mode="wb", mtime=epoch, filename="") as gz:
        gz.write(data.getvalue())


if __name__ == "__main__":
    main()
