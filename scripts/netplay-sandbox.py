#!/usr/bin/env python3
"""Pay a server-issued Sandbox entry, or independently audit its receipts.

Example: netplay-sandbox.py --state /.../SatsArenaSandbox/ID/prepaid-match.json --slot 0
Credentials come only from the isolated agent fixture and reach NWC over stdin.
This helper never writes a financial ledger and never creates an entry invoice.
"""
import argparse
import json
from pathlib import Path
import stat
import subprocess


ROOT = Path(__file__).resolve().parents[1]
FIXTURE = Path('/home/deck/.local/share/sats-arena/agent/Slippi Launcher/sats-arena/sandbox/wallets.json')
RUST = ROOT.parent / 'Ishiiruka/build-tools'


class SafeError(Exception):
    """Messages that contain no fixture or provider data."""


def require(condition, message):
    if not condition:
        raise SafeError(message)


def validate_state(path, saved, wallets, slot=None):
    require('SatsArenaSandbox' in path.parts and path.name == 'prepaid-match.json',
            'Choose a prepaid-match.json inside SatsArenaSandbox')
    require(saved.get('schema') == 5 and saved.get('simulated') is False
            and saved.get('host_mode') == 'sandbox', 'Only server-hosted Sandbox state is allowed')
    identity = saved.get('wallet_identity', '')
    require(isinstance(identity, str) and len(identity) == 64
            and all(c in '0123456789abcdef' for c in identity), 'Missing wallet fingerprint')
    pot = saved.get('pot', {})
    match_id = pot.get('id', '')
    require(isinstance(match_id, str) and len(match_id) == 32
            and all(c in '0123456789abcdef' for c in match_id), 'Invalid financial match ID')
    kinds = pot.get('kinds')
    require(kinds in ([0, 0, 3, 3], [0, 0, 0, 3], [0, 0, 0, 0])
            and all(type(kind) is int for kind in kinds),
            'Expected two to four human players in consecutive slots')
    count = kinds.count(0)
    teams, team_ids = pot.get('teams'), pot.get('team_ids')
    require(type(teams) is bool and isinstance(team_ids, list) and len(team_ids) == 4
            and all(type(team) is int and
                    (0 <= team <= 2 if teams and i < count else team == 255)
                    for i, team in enumerate(team_ids))
            and (not teams or len(set(team_ids[:count])) >= 2),
            'Invalid funded FFA or team assignment')
    addresses = saved.get('addresses', [])
    require(len(addresses) == 4 and all(not address or address == wallets[i + 1]['address']
                                      for i, address in enumerate(addresses[:count])),
            'A receiver does not match its isolated player wallet')
    if slot is not None:
        require(type(slot) is int and 0 <= slot < count, 'Choose an active player slot (0 through 3)')
        require(not saved.get('frozen') and pot.get('phase') in ('Funding', 'Ready'),
                'Only an unfrozen, unstarted match may be funded')
        require(pot.get('paid', [])[slot] is False, 'This entry is already confirmed')
        require(saved.get('addresses', [])[slot] == wallets[slot + 1]['address'],
                'Entry receiver does not match the isolated player wallet')
        invoice = saved.get('invoices', [])[slot]
        require(isinstance(invoice, str) and invoice.lower().startswith('lnbc'),
                'Wait for the server-issued BOLT11 entry invoice')
    return pot


def command(example):
    return ['distrobox', 'enter', '-n', 'slippi-arena-build', '--no-tty', '--',
            str(ROOT / 'target/debug/examples' / example)]


def run_example(example, payload, allow_report=False):
    try:
        result = subprocess.run(command(example), input=json.dumps(payload), text=True,
                                capture_output=True, timeout=180)
    except subprocess.TimeoutExpired:
        raise SafeError('Sandbox tool timed out; inspect receipts before any retry') from None
    # Provider errors can include connection details. Keep them out of console logs.
    require(result.returncode == 0 or (allow_report and result.stdout.lstrip().startswith('{')),
            f'{example} failed; funds may be pending, inspect receipts before retrying')
    return result.stdout


def audit(path):
    result = run_example('sandbox_receipts', {'fixture': str(FIXTURE), 'state': str(path)},
                         allow_report=True)
    try:
        return json.loads(result)
    except ValueError:
        raise SafeError('Receipt tool returned an invalid report') from None


def self_test():
    wallets = [{'address': address} for address in ('host', 'one', 'two', 'three', 'four')]
    path = Path('/isolated/SatsArenaSandbox/room/prepaid-match.json')
    saved = {'schema': 5, 'simulated': False, 'host_mode': 'sandbox',
             'wallet_identity': 'a' * 64, 'frozen': False,
             'pot': {'id': 'b' * 32, 'phase': 'Funding', 'kinds': [0, 0, 3, 3],
                     'teams': False, 'team_ids': [255] * 4, 'paid': [False] * 4},
             'addresses': ['one', 'two', '', ''], 'invoices': ['lnbc-example'] * 4}
    validate_state(path, saved, wallets, 0)
    for count in range(2, 5):
        for teams in (False, True):
            roster = dict(saved['pot'], kinds=[0] * count + [3] * (4 - count), teams=teams,
                          team_ids=[i % 2 if teams and i < count else 255 for i in range(4)])
            expanded = dict(saved, pot=roster,
                            addresses=[wallets[i + 1]['address'] if i < count else '' for i in range(4)])
            for slot in range(count):
                validate_state(path, expanded, wallets, slot)
            validate_state(path, expanded, wallets)  # Read-only audit accepts the same safe rosters.
    for invalid_path, patch, slot in [
        (Path('/ordinary/prepaid-match.json'), {}, 0),
        (path, {'host_mode': 'nwc'}, 0),
        (path, {'simulated': True}, 0),
        (path, {'frozen': True}, 0),
        (path, {}, 2),
        (path, {'addresses': ['other', 'two', '', '']}, 0),
        (path, {'invoices': ['', '', '', '']}, 0),
        (path, {'pot': dict(saved['pot'], paid=[True, False, False, False])}, 0),
        (path, {'pot': dict(saved['pot'], phase='Playing')}, 0),
        (path, {}, -1),
        (path, {}, 4),
        (path, {'pot': dict(saved['pot'], kinds=[0, 3, 0, 3])}, 0),
        (path, {'pot': dict(saved['pot'], kinds=[0, 1, 3, 3])}, 0),
        (path, {'pot': dict(saved['pot'], kinds=[False, 0, 3, 3])}, 0),
        (path, {'pot': dict(saved['pot'], kinds=[0, 3, 3, 3])}, 0),
        (path, {'pot': dict(saved['pot'], teams=True, team_ids=[0, 0, 255, 255])}, 0),
        (path, {'pot': dict(saved['pot'], teams=True, team_ids=[0, 3, 255, 255])}, 0),
        (path, {'pot': dict(saved['pot'], teams=True, team_ids=[0, 1, 0, 255])}, 0),
        (path, {'pot': dict(saved['pot'], team_ids=[0, 1, 255, 255])}, 0),
        (path, {'pot': dict(saved['pot'], teams=1)}, 0),
        (path, {'pot': dict(saved['pot'], kinds=[0] * 4),
                'addresses': ['one', 'two', 'wrong', 'four']}, 2),
        (path, {'pot': dict(saved['pot'], kinds=[0] * 4),
                'addresses': ['one', 'two', 'three', 'wrong']}, 3),
    ]:
        try:
            validate_state(invalid_path, dict(saved, **patch), wallets, slot)
        except SafeError:
            continue
        raise AssertionError('Unsafe Sandbox input accepted')
    print('Sandbox safety checks passed; no wallets accessed')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--state', type=Path, help='Server prepaid-match.json (read only)')
    mode = parser.add_mutually_exclusive_group(required=True)
    mode.add_argument('--slot', type=int, choices=range(4), help='Pay this zero-based active player entry')
    mode.add_argument('--audit', action='store_true', help='Read-only incoming/outgoing receipt audit')
    mode.add_argument('--self-test', action='store_true', help='Check safety guards without wallet access')
    parser.add_argument('--no-build', action='store_true', help='Use already compiled Rust examples')
    args = parser.parse_args()
    if args.self_test:
        self_test()
        return
    require(args.state is not None, '--state is required')
    path = args.state.resolve(strict=True)
    # Validate location before opening either financial state or a credential fixture.
    require('SatsArenaSandbox' in path.parts and path.name == 'prepaid-match.json',
            'Choose a prepaid-match.json inside SatsArenaSandbox')
    metadata = FIXTURE.lstat()
    require(stat.S_ISREG(metadata.st_mode) and metadata.st_mode & 0o077 == 0,
            'The dedicated Sandbox fixture must be a private regular file')
    fixture = json.loads(FIXTURE.read_text())
    wallets = fixture.get('wallets', [])
    require(fixture.get('version') == 1 and len(wallets) == 5,
            'The isolated Sandbox fixture is incomplete')
    saved = json.loads(path.read_text())
    pot = validate_state(path, saved, wallets, args.slot)
    if not args.no_build:
        result = subprocess.run(
            ['distrobox', 'enter', '-n', 'slippi-arena-build', '--no-tty', '--', 'env',
             f'CARGO_HOME={RUST / "cargo"}', f'RUSTUP_HOME={RUST / "rustup"}',
             'RUSTUP_TOOLCHAIN=1.88.0', str(RUST / 'cargo/bin/cargo'), 'build', '--quiet',
             '--manifest-path', str(ROOT / 'Cargo.toml'), '-p', 'sats-arena-core',
             '--example', 'nwc_sandbox', '--example', 'sandbox_receipts'],
            capture_output=True, timeout=300)
        require(result.returncode == 0, 'Could not build the existing Sandbox tools')
    # This read-only NWC audit verifies fixture0's wallet fingerprint before any send.
    report = audit(path)
    require(report.get('match_id') == pot['id'] and report.get('simulated') is False,
            'Audit did not confirm the expected Sandbox match')
    if args.audit:
        print(json.dumps(report, indent=2))
        require(report.get('all_payouts_confirmed') is True, 'Some payouts remain unresolved')
        return
    require(report.get('all_payouts_confirmed') is True,
            'Resolve the existing payment audit before funding')
    # Reload after the network preflight so a concurrently closed/changed lobby is rejected.
    current = json.loads(path.read_text())
    validate_state(path, current, wallets, args.slot)
    require(current['pot']['id'] == pot['id']
            and current['invoices'][args.slot] == saved['invoices'][args.slot]
            and current['wallet_identity'] == saved['wallet_identity'],
            'Entry changed during preflight; inspect the current lobby')
    output = run_example('nwc_sandbox', {'arena_uri': wallets[0]['uri'],
                                       'payer_uri': wallets[args.slot + 1]['uri'],
                                       'invoice': current['invoices'][args.slot]})
    require('incoming receipt confirmed' in output, 'Incoming receipt was not confirmed')
    entry = current['pot']['entry_sats']
    surcharge = (entry * current['pot'].get('surcharge_bps', 0) + 9999) // 10000
    print(json.dumps({'match_id': pot['id'], 'slot': args.slot, 'sats': entry + surcharge,
                      'incoming_receipt_confirmed': True, 'simulated': False}))


if __name__ == '__main__':
    try:
        main()
    except SafeError as error:
        raise SystemExit(str(error)) from None
    except (ValueError, KeyError, IndexError, TypeError, OSError, subprocess.SubprocessError):
        # Never echo arbitrary fixture/provider contents, including malformed JSON.
        raise SystemExit('Sandbox helper failed; verify inputs/build/network and inspect receipts before retrying') from None
