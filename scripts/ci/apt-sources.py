#!/usr/bin/env python3
"""Select a direct Ubuntu mirror, including GitHub runners' mirror+file sources."""
from pathlib import Path
import re
import sys


def rewrite(text, mirror):
    return re.subn(
        r'(?:https?://[^\s]+/ubuntu/?|mirror\+file:/etc/apt/apt-mirrors(?:-security)?\.txt)(?=\s|$)',
        mirror, text,
    )


def main():
    sources = [Path('/etc/apt/sources.list'), *Path('/etc/apt/sources.list.d').glob('*.list'),
               *Path('/etc/apt/sources.list.d').glob('*.sources')]
    changed = 0
    for source in sources:
        if not source.exists():
            continue
        text, count = rewrite(source.read_text(), sys.argv[1])
        if count:
            source.write_text(text)
            changed += count
            print(f'{source}: selected {sys.argv[1]} for {count} Ubuntu origins')
    if not changed:
        raise RuntimeError('No recognized Ubuntu sources; refusing to silently retain an unknown mirror')


if __name__ == '__main__':
    main()
