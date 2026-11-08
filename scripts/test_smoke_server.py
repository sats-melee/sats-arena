import importlib.util
from pathlib import Path
import sys
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location('smoke_server', Path(__file__).with_name('smoke-server.py'))
smoke_server = importlib.util.module_from_spec(spec)
spec.loader.exec_module(smoke_server)


class ServerSmokeTest(unittest.TestCase):
    def setUp(self):
        self.process_patch = patch.object(smoke_server.subprocess, 'Popen')
        self.process = self.process_patch.start().return_value
        self.addCleanup(self.process_patch.stop)
        self.process.poll.return_value = None
        self.http_patch = patch.object(smoke_server.http.client, 'HTTPConnection')
        self.connection = self.http_patch.start().return_value
        self.addCleanup(self.http_patch.stop)
        response = self.connection.getresponse.return_value
        response.status = 200
        response.read.return_value = b'{"ok": true, "service": "sats-melee-rooms", "version": 1}'

    def test_healthy_server_is_stopped(self):
        smoke_server.smoke(sys.executable)
        self.process.terminate.assert_called_once()
        self.process.wait.assert_called_once_with(timeout=5)

    def test_wrong_service_fails_and_stops_child(self):
        self.connection.getresponse.return_value.read.return_value = b'{"ok": true}'
        with self.assertRaisesRegex(RuntimeError, 'unexpected health response'):
            smoke_server.smoke(sys.executable)
        self.process.terminate.assert_called_once()

    def test_loader_exit_is_not_mistaken_for_a_healthy_server(self):
        self.process.poll.return_value = 42
        self.process.returncode = 42
        with self.assertRaisesRegex(RuntimeError, 'status 42'):
            smoke_server.smoke(sys.executable)
        self.connection.request.assert_not_called()
        self.process.wait.assert_called_once_with(timeout=5)

    def test_unresponsive_server_is_stopped_after_timeout(self):
        with patch.object(smoke_server.time, 'monotonic', side_effect=[0, 16]):
            with self.assertRaisesRegex(RuntimeError, 'timeout'):
                smoke_server.smoke(sys.executable)
        self.process.terminate.assert_called_once()


if __name__ == '__main__':
    unittest.main()
