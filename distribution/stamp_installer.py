"""Write the one-command installer for a release, set to install that release."""

import argparse
from pathlib import Path

import package

SOURCE = Path(__file__).resolve().with_name("install.sh")
PLACEHOLDER = "\nrelease=latest\n"


def stamp(text, release_version):
    if not package.SEMVER.fullmatch(release_version):
        raise ValueError("invalid release version")
    if text.count(PLACEHOLDER) != 1:
        raise ValueError("installer has no single release line to set")
    return text.replace(PLACEHOLDER, f"\nrelease={release_version}\n")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    args.output.write_text(stamp(SOURCE.read_text(), package.version()))
    args.output.chmod(0o755)
    print(args.output)


if __name__ == "__main__":
    main()
