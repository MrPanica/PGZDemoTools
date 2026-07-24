# -*- mode: python ; coding: utf-8 -*-

import sys
from pathlib import Path


root = Path(SPECPATH)
suffix = ".exe" if sys.platform == "win32" else ""
release = root / "helper" / "target" / "release"

a = Analysis(
    ['demo_tools.py'],
    pathex=[],
    binaries=[
        (str(release / f"voice_extract{suffix}"), '.'),
        (str(release / f"pov_cut{suffix}"), '.'),
    ],
    datas=[
        (str(root / 'lame.min.js'), '.'),
        (str(root / 'LAMEJS-LICENSE.txt'), '.'),
    ],
    hiddenimports=[],
    hookspath=[],
    hooksconfig={},
    runtime_hooks=[],
    excludes=[],
    noarchive=False,
    optimize=0,
)
pyz = PYZ(a.pure)

exe = EXE(
    pyz,
    a.scripts,
    a.binaries,
    a.datas,
    [],
    name='PGZDemoTools',
    debug=False,
    bootloader_ignore_signals=False,
    strip=False,
    upx=True,
    upx_exclude=[],
    runtime_tmpdir=None,
    console=True,
    disable_windowed_traceback=False,
    argv_emulation=False,
    target_arch=None,
    codesign_identity=None,
    entitlements_file=None,
)
