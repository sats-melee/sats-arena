#!/usr/bin/env python3
"""Exercise two actual room helpers against a running local room server."""
import argparse
import json
import os
import select
from pathlib import Path
import subprocess
import tempfile


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--server', default='http://127.0.0.1:8738')
    args = parser.parse_args()
    helper = Path(__file__).with_name('netplay-room.py')
    base = ['python3', str(helper), '--server', args.server,
            '--build', 'cli-self-test', '--rules', 'vanilla-test']
    with tempfile.TemporaryDirectory(prefix='arena-room-test-') as folder:
        root = Path(folder)
        child = ('import os,time; '
                 "assert os.environ['SATS_ARENA_FAKE_LOCAL']=='0'; "
                 "assert 'SATS_ARENA_NWC_URI' not in os.environ; "
                 "assert os.path.isfile(os.environ['SATS_ARENA_NETPLAY_FILE']); "
                 'time.sleep(3)')
        env = dict(os.environ, SATS_ARENA_NWC_URI='invalid-test-marker', SATS_ARENA_FAKE_LOCAL='1')
        processes = []
        logs = []
        try:
            for index in range(2):
                log = (root / f'{index}.log').open('w+')
                logs.append(log)
                command = base + ['--name', f'Player {index}', '--endpoint', f'127.0.0.1:{45113 + index}',
                                  '--descriptor', str(root / f'{index}.json')]
                if index:
                    command += ['--join', code]
                process = subprocess.Popen(command + ['--', 'python3', '-c', child],
                                           env=env, stdout=subprocess.PIPE, stderr=log, text=True)
                processes.append(process)
                # The timeout below also bounds failure before room admission.
                if not index:
                    assert select.select([process.stdout], [], [], 10)[0], 'No admission response'
                    code = json.loads(process.stdout.readline())['code']
            for process in processes:
                process.communicate(timeout=20)
            assert processes[0].returncode == 0
            # The guest can observe the host closing after its child exits.
            assert processes[1].returncode in (0, 1)
            snapshots = [json.loads((root / f'{i}.json').read_text()) for i in range(2)]
            assert [s['slot'] for s in snapshots] == [0, 1]
            assert snapshots[0]['room'] == snapshots[1]['room']
            for index, log in enumerate(logs):
                assert (root / f'{index}.json').stat().st_mode & 0o777 == 0o600
                log.seek(0)
                assert 'AssertionError' not in log.read(), 'Child environment check failed'
            print('PASS: shared room, distinct slots, private descriptors, payment host disabled, wallet variable removed')
        finally:
            for process in processes:
                if process.poll() is None:
                    process.terminate()
                    process.wait(timeout=12)
            for log in logs:
                log.close()


if __name__ == '__main__':
    main()
