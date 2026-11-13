"""Read the files inside a release archive without extracting them."""

import gzip
import tarfile
from pathlib import Path


def read(path):
    with Path(path).open("rb") as raw, gzip.GzipFile(fileobj=raw) as compressed:
        with tarfile.open(fileobj=compressed, mode="r") as archive:
            files = {}
            for member in archive.getmembers():
                if not member.isfile() or member.name in files or "/" in member.name:
                    raise ValueError("release archive entry is not a single file")
                extracted = archive.extractfile(member)
                if extracted is None:
                    raise ValueError("release archive entry is unreadable")
                files[member.name] = extracted.read()
            return files
