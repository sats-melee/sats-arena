#!/usr/bin/env python3
"""Join a no-payment LAN room, keep its lease alive, and launch an emulator."""
import argparse
import json
import os
from pathlib import Path
import signal
import subprocess
import time
import urllib.error
import urllib.parse
import urllib.request


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--server', default='http://127.0.0.1:8738')
    parser.add_argument('--join', metavar='CODE', help='Omit to create a room')
    parser.add_argument('--name', required=True)
    parser.add_argument('--endpoint', required=True, help='This emulator’s reachable IPv4:UDP-port')
    parser.add_argument('--build', required=True)
    parser.add_argument('--rules', required=True)
    parser.add_argument('--descriptor', required=True, type=Path)
    parser.add_argument('command', nargs=argparse.REMAINDER, help='Optional emulator command after --')
    args = parser.parse_args()
    if args.descriptor.exists():
        parser.error('Choose a new descriptor path; existing files are never overwritten')
    url = urllib.parse.urlsplit(args.server)
    if url.scheme not in ('http', 'https') or not url.hostname or url.username or url.password or url.query or url.fragment:
        parser.error('Use an HTTP(S) server URL without credentials, query, or fragment')
    if url.scheme == 'http' and url.hostname not in ('localhost', '127.0.0.1', '::1'):
        parser.error('Use HTTPS for a server outside loopback')
    base = args.server.rstrip('/') + '/netplay/rooms'
    token = None
    child = None
    path = None
    stopping = False
    def stop(_signal, _frame):
        nonlocal stopping
        stopping = True
    signal.signal(signal.SIGTERM, stop)
    signal.signal(signal.SIGINT, stop)

    def call(url, body=None):
        headers = {'Content-Type': 'application/json'}
        if token:
            headers['Authorization'] = 'Bearer ' + token
        request = urllib.request.Request(url, headers=headers, data=None if body is None else json.dumps(body).encode())
        try:
            with urllib.request.urlopen(request, timeout=5) as response:
                return json.load(response)
        except urllib.error.HTTPError as error:
            raise RuntimeError(f'Room request failed (HTTP {error.code})') from None

    try:
        admission = call(base + '/' + urllib.parse.quote(args.join, safe='') + '/join' if args.join else base,
                         {'name': args.name, 'endpoint': args.endpoint, 'build': args.build, 'rules': args.rules})
        token = admission['token']
        slot = admission['slot']
        path = base + '/' + admission['room']['code']
        print(json.dumps({'code': admission['room']['code'], 'slot': slot}), flush=True)
        call(path, {'type': 'ready', 'ready': True})
        launched = False
        while not stopping:
            room = call(path)
            # Roster changes deliberately clear readiness; acknowledge the current LAN-test roster.
            if room['match_id'] is None:
                if len(room['players']) > 2:
                    raise RuntimeError('This emulator integration currently supports two players only')
                if not next(p for p in room['players'] if p['slot'] == slot)['ready']:
                    call(path, {'type': 'ready', 'ready': True})
                elif slot == 0 and len(room['players']) == 2 and all(p['ready'] for p in room['players']):
                    room = call(path, {'type': 'start'})
            if room['match_id'] is not None and not launched:
                args.descriptor.parent.mkdir(parents=True, exist_ok=True)
                # Exclusive creation prevents overwriting a file or following a symlink.
                fd = os.open(args.descriptor, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
                with os.fdopen(fd, 'w') as stream:
                    json.dump({'slot': slot, 'room': room}, stream)
                command = args.command[1:] if args.command[:1] == ['--'] else args.command
                if command:
                    env = dict(os.environ, SATS_ARENA_NETPLAY_FILE=str(args.descriptor.resolve()))
                    # Developer netplay never inherits a wallet connection from the shell.
                    for key in list(env):
                        if key.startswith('SATS_ARENA_') and key != 'SATS_ARENA_NETPLAY_FILE':
                            del env[key]
                    env['SATS_ARENA_FAKE_LOCAL'] = '0'
                    child = subprocess.Popen(command, env=env)
                launched = True
                print(json.dumps({'started': room['match_id'], 'slot': slot}), flush=True)
            if child is not None and child.poll() is not None:
                return child.returncode
            time.sleep(1 if not launched else 5)
    finally:
        if child is not None and child.poll() is None:
            child.terminate()
            try:
                child.wait(timeout=10)
            except subprocess.TimeoutExpired:
                child.kill()
                child.wait()
        if path:
            try:
                call(path, {'type': 'leave'})
            except (RuntimeError, urllib.error.URLError, TimeoutError):
                pass
    return 0


if __name__ == '__main__':
    try:
        raise SystemExit(main())
    except (RuntimeError, OSError, ValueError, KeyError) as error:
        raise SystemExit(str(error)) from None
