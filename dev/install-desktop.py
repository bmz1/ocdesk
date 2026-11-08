#!/usr/bin/env python3
"""Register a source/local build with Linux desktops, including Wayland.

Usage: python3 dev/install-desktop.py [target/debug/ocdesk] [--data-dir PATH]
Packaged tarballs already include install.sh for the same desktop identity.
"""
import argparse
import os
from pathlib import Path
import shutil
import subprocess
import tempfile

APP_ID = 'io.github.bmz1.ocdesk'
ROOT = Path(__file__).resolve().parents[1]


def desktop_exec(binary):
    value = str(binary)
    if '\n' in value or '\r' in value:
        raise ValueError('Executable path must not contain a newline')
    if '%' in value or '=' in value:
        raise ValueError('Use an executable path without percent signs or equals signs for desktop registration')
    # Exec quoting happens after Desktop Entry string unescaping.
    value = value.replace('\\', '\\\\').replace('"', '\\"').replace('`', '\\`').replace('$', '\\$')
    return ('"' + value + '" %f').replace('\\', '\\\\')


def atomic_write(path, data):
    path.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.NamedTemporaryFile(dir=path.parent, prefix='.ocdesk-', delete=False) as output:
        temporary = Path(output.name)
        try:
            output.write(data)
            output.flush()
            os.fchmod(output.fileno(), 0o644)
            temporary.replace(path)
        finally:
            temporary.unlink(missing_ok=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('binary', nargs='?', type=Path, default=ROOT / 'target/debug/ocdesk')
    parser.add_argument('--data-dir', type=Path, default=Path(os.environ.get('XDG_DATA_HOME', Path.home() / '.local/share')))
    args = parser.parse_args()
    binary = args.binary.expanduser().resolve()
    if not binary.is_file() or not os.access(binary, os.X_OK):
        parser.error(f'Build ocdesk first; executable not found: {binary}')
    entry = (ROOT / f'packaging/linux/{APP_ID}.desktop').read_text()
    try:
        entry = '\n'.join('Exec=' + desktop_exec(binary) if line.startswith('Exec=') else line for line in entry.splitlines()) + '\n'
    except ValueError as error:
        parser.error(str(error))
    data_dir = args.data_dir.expanduser().resolve()
    desktop = data_dir / 'applications' / f'{APP_ID}.desktop'
    icon = data_dir / 'icons/hicolor/1024x1024/apps' / f'{APP_ID}.png'
    atomic_write(icon, (ROOT / 'packaging/icon.png').read_bytes())
    atomic_write(desktop, entry.encode())
    for tool, extra in [('update-desktop-database', [str(desktop.parent)]),
                        ('gtk-update-icon-cache', ['-q', '-f', '-t', str(data_dir / 'icons/hicolor')])]:
        if shutil.which(tool):
            subprocess.run([tool, *extra], check=False, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    print(f'Installed launcher: {desktop}')
    print(f'Installed icon: {icon}')


if __name__ == '__main__':
    main()
