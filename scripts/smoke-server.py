#!/usr/bin/env python3
"""Start a packaged room server on loopback and verify it answers health checks."""
import http.client
import json
from pathlib import Path
import socket
import subprocess
import sys
import tempfile
import time


def smoke(binary, timeout=15):
    binary = Path(binary).resolve(strict=True)
    with socket.socket() as listener:
        listener.bind(('127.0.0.1', 0))
        port = listener.getsockname()[1]
    with tempfile.TemporaryDirectory(prefix='arena-server-smoke-') as directory:
        with tempfile.TemporaryFile(mode='w+b') as log:
            child = subprocess.Popen(
                [str(binary), 'rooms', '--bind', f'127.0.0.1:{port}', '--frontend', directory],
                cwd=directory, stdout=log, stderr=subprocess.STDOUT,
            )
            try:
                deadline = time.monotonic() + timeout
                while time.monotonic() < deadline:
                    if child.poll() is not None:
                        raise RuntimeError(f'Server exited with status {child.returncode}')
                    connection = http.client.HTTPConnection('127.0.0.1', port, timeout=0.5)
                    try:
                        connection.request('GET', '/health')
                        response = connection.getresponse()
                        value = json.loads(response.read())
                        if response.status != 200 or value != {
                            'ok': True, 'service': 'sats-melee-rooms', 'version': 1,
                        }:
                            raise RuntimeError('Server returned an unexpected health response')
                        print(f'{binary.name}: loopback /health passed')
                        return
                    except OSError:
                        time.sleep(0.1)
                    finally:
                        connection.close()
                raise RuntimeError('Server did not become healthy before the timeout')
            except Exception:
                log.seek(0)
                print(log.read().decode(errors='replace'), file=sys.stderr)
                raise
            finally:
                if child.poll() is None:
                    child.terminate()
                try:
                    child.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    child.kill()
                    child.wait(timeout=5)


if __name__ == '__main__':
    smoke(sys.argv[1])
