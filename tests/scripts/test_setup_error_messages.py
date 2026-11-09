"""Errors that previously sent users in the wrong direction (offline)."""
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / 'scripts'))
import launcher_setup
from roblox_session import SessionError
import terminal_client


class FakeSession:
    reply = {}

    def __init__(self, *, cookie):
        self._cookie = cookie

    def request(self, host, path):
        return dict(self.reply)


def session(reply):
    return type('Fake', (FakeSession,), {'reply': reply})


class ConnectAccountMessageTests(unittest.TestCase):
    def connect(self, reply, value='x' * 1100):
        with tempfile.TemporaryDirectory() as root:
            with self.assertRaises(SessionError) as caught:
                launcher_setup.connect_account(root, value, session_factory=session(reply))
            self.assertFalse((Path(root) / 'secrets/roblox-cookie.txt').exists())
        message = str(caught.exception)
        self.assertNotIn(value, message)  # never echo the credential
        return message

    def test_challenge_is_named(self):
        self.assertIn('verification', self.connect({'http_status': 403, 'challenge_present': True}))

    def test_signed_out_session_is_named(self):
        self.assertIn('signed out or expired', self.connect({'http_status': 401, 'challenge_present': False}))

    def test_truncated_paste_gets_a_length_hint(self):
        message = self.connect({'http_status': 401, 'challenge_present': False}, value='y' * 120)
        self.assertIn('120 characters', message)
        self.assertNotIn('characters', self.connect({'http_status': 401, 'challenge_present': False}))

    def test_other_failures_report_status(self):
        self.assertIn('HTTP 500', self.connect({'http_status': 500, 'challenge_present': False}))
        self.assertIn('HTTP 200', self.connect({'http_status': 200, 'challenge_present': False, 'data': {}}))


class TargetSelectionMessageTests(unittest.TestCase):
    def prepare(self, stdout):
        result = subprocess.CompletedProcess([], 1, stdout=stdout, stderr='')
        with patch.object(terminal_client, 'ensure_console_free'), \
                patch.object(terminal_client.subprocess, 'run', return_value=result), \
                patch.object(terminal_client, 'log'):
            with self.assertRaises(SessionError) as caught:
                terminal_client.prepare_target('123')
        return str(caught.exception)

    def test_local_crash_is_not_blamed_on_the_account(self):
        # Regression: a missing outputs/ folder surfaced as "check account access".
        message = self.prepare('{"error_type": "FileNotFoundError"}')
        self.assertIn('crashed locally (FileNotFoundError)', message)
        self.assertNotIn('account', message)

    def test_selector_messages_pass_through(self):
        self.assertEqual(self.prepare('{"error": "Place is private"}'), 'Place is private')

    def test_unparseable_output_keeps_generic_message(self):
        self.assertIn('Check account access', self.prepare('not json'))
        self.assertIn('Check account access', self.prepare('[1, 2]'))
        self.assertIn('Check account access', self.prepare('{"error_type": "bad name!"}'))


if __name__ == '__main__':
    unittest.main()
