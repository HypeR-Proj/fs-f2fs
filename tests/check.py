#!/usr/bin/env python3
# SPDX-FileCopyrightText: 2026 roolrz
# SPDX-License-Identifier: Apache-2.0
"""Build and exercise the actual Fuchsia media algorithms on the host."""
from pathlib import Path
import os
import subprocess
import sys
component = Path(__file__).resolve().parents[1]
root = component.parents[1]
build_output = Path(os.environ.get('HYPER_BUILD_OUTPUT', os.environ.get('OUT', root / 'out'))).resolve()
output = build_output / 'build/fs-f2fs/host'
subprocess.run([sys.executable, str(component / 'build.py'), '--host', '--output',
                str(output)], check=True)
subprocess.run([sys.executable, str(component / 'tests/host.py'),
                str(output / 'libhyper-f2fs.so')], check=True)

subprocess.run([sys.executable, str(component / "tests/format.py"),
                str(output / "libhyper-f2fs.so")], check=True)
subprocess.run([sys.executable, str(component / 'tests/recovery.py'),
                str(output / 'libhyper-f2fs.so')], check=True)
