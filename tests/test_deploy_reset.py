"""Exercise the real deployment reset block with mocked OS controls, no live files."""
import os
from pathlib import Path
import subprocess
import tempfile
import unittest

DEPLOY = Path(__file__).resolve().parents[1] / 'deploy.sh'

class ResetTests(unittest.TestCase):
    def run_reset(self, deny_stop=False, process_running=False):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            runtime = root / 'runtime'
            runtime.mkdir()
            (runtime / 'data.db').write_text('stale')
            (runtime / 'paper_xs.json').write_text('stale')
            (runtime / 'bin').mkdir()
            (runtime / 'bin' / 'bot').write_text('preserve')
            mock = root / 'mock'
            mock.mkdir()
            launch = '#!/bin/sh\n'
            if deny_stop:
                launch += '[ "$1" = print ] && exit 0\nexit 1\n'
            else:
                launch += 'exit 1\n'
            (mock / 'launchctl').write_text(launch)
            (mock / 'pgrep').write_text('#!/bin/sh\nexit '+ ('0' if process_running else '1')+'\n')
            for path in mock.iterdir():
                path.chmod(0o755)
            fetch = root / 'target/release/examples/fetch_research_data'
            fetch.parent.mkdir(parents=True)
            fetch.write_text('#!/bin/sh\n[ ! -e "$RUNTIME/data.db" ] || exit 9\ntouch "$RUNTIME/fetched"\n')
            fetch.chmod(0o755)
            source = DEPLOY.read_text()
            block = source[source.index('# NOTE(agents): EVERY deployment'):source.index('mkdir -p "$RUNTIME/bin"')]
            env = dict(os.environ, PATH=str(mock)+os.pathsep+os.environ['PATH'], RUNTIME=str(runtime), LABEL='test.bot')
            result = subprocess.run(['bash','-c','set -euo pipefail\ncheck_review() { :; }\n'+block], cwd=root, env=env, capture_output=True)
            return result.returncode, (runtime/'data.db').exists(), (runtime/'fetched').exists(), (runtime/'bin/bot').read_text()

    def test_denied_stop_preserves_active_data(self):
        code, stale, fetched, binary = self.run_reset(deny_stop=True)
        self.assertNotEqual(code, 0)
        self.assertTrue(stale)
        self.assertFalse(fetched)
        self.assertEqual(binary, 'preserve')

    def test_remaining_process_preserves_active_data(self):
        code, stale, fetched, _ = self.run_reset(process_running=True)
        self.assertNotEqual(code, 0)
        self.assertTrue(stale)
        self.assertFalse(fetched)

    def test_stopped_service_deletes_before_refetch(self):
        code, stale, fetched, binary = self.run_reset()
        self.assertEqual(code, 0)
        self.assertFalse(stale)
        self.assertTrue(fetched)
        self.assertEqual(binary, 'preserve')

if __name__ == '__main__':
    unittest.main()
