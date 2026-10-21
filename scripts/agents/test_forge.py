"""Offline transport and custody checks: python3 -m unittest discover -s scripts/agents."""
import contextlib
import importlib.util
import io
import json
import os
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch
import urllib.error

spec = importlib.util.spec_from_file_location('forge', Path(__file__).with_name('forge.py'))
forge = importlib.util.module_from_spec(spec)
spec.loader.exec_module(forge)


class ForgeTests(unittest.TestCase):
    def test_actions_use_bearer_and_exact_api_contracts(self):
        with tempfile.TemporaryDirectory() as folder:
            directory = Path(folder)
            token = directory / 'agent-forge.token'
            token.write_text('secret-test-token\n')
            token.chmod(0o600)
            body = directory / 'body'
            body.write_text('a comment or PR body')
            cases = [
                (['open', '--head', 'topic', '--base', 'main', '--title', 'Title',
                  '--body-file', str(body)], 'POST', '/pulls',
                 dict(head='topic', base='main', title='Title', body=body.read_text(), draft=True)),
                (['comment', '3', '--head-sha', 'a' * 40, '--body-file', str(body)], 'POST', '/pulls/3/reviews',
                 {'verdict': 'comment', 'expected_head_sha': 'a' * 40,
                  'body_markdown': body.read_text(), 'thread_comments': []}),
                (['close-as-replaced', '3', '--replacement', '4'], 'POST', '/pulls/3/close',
                 {'superseded_by': 4}),
                (['ready', '3'], 'POST', '/pulls/3/ready', {}),
                (['reviews', '3'], 'GET', '/pulls/3', None),
            ]
            for argv, method, route, payload in cases:
                with self.subTest(argv=argv), patch.dict(os.environ, {
                    'JERYU_FORGE_URL': 'https://forge.invalid',
                    'JERYU_CREDENTIALS_DIR': folder}), patch.object(
                        forge.urllib.request, 'build_opener') as build, patch.object(
                        forge.sys, 'argv', ['forge', '--repo', 'team/repo'] + argv), contextlib.redirect_stdout(io.StringIO()):
                    build.return_value.open.return_value.__enter__.return_value.read.return_value = b'{}'
                    self.assertEqual(forge.main(), 0)
                    req = build.return_value.open.call_args.args[0]
                    self.assertEqual(req.full_url, 'https://forge.invalid/api/' + ('v3' if argv[0] == 'open' else 'v1') + '/repos/team/repo' + route)
                    self.assertEqual(req.method, method)
                    self.assertEqual(req.get_header('Authorization'), 'Bearer secret-test-token')
                    self.assertEqual(None if req.data is None else json.loads(req.data), payload)
                    self.assertEqual(build.return_value.open.call_count, 1)

    def test_approval_and_merge_are_not_commands(self):
        for action in ('approve', 'merge'):
            with self.subTest(action=action), contextlib.redirect_stderr(io.StringIO()):
                with self.assertRaises(SystemExit):
                    forge.parser().parse_args(['--repo', 'team/repo', action, '1'])

    def test_custody_and_path_validation(self):
        with tempfile.TemporaryDirectory() as folder:
            path = Path(folder) / 'token'
            path.write_text('secret')
            path.chmod(0o644)
            with self.assertRaises(ValueError):
                forge.read_token(path)
            path.chmod(0o600)
            self.assertEqual(forge.read_token(path), 'secret')
            link = Path(folder) / 'link'
            link.symlink_to(path)
            with self.assertRaises(OSError):
                forge.read_token(link)
        args = forge.parser().parse_args(['--repo', '../repo', 'reviews', '1'])
        with self.assertRaises(ValueError):
            forge.request_spec(args)
        with self.assertRaises(ValueError):
            forge.NoRedirect().redirect_request(None, None, 302, '', {}, 'https://elsewhere.invalid')

    def test_forbidden_is_not_retried_and_does_not_disclose_body(self):
        with patch.dict(os.environ, {'JERYU_FORGE_URL': 'https://forge.invalid',
              'JERYU_CREDENTIALS_DIR': '/unused'}), patch.object(forge, 'read_token', return_value='secret'), patch.object(
                forge.sys, 'argv', ['forge', '--repo', 'team/repo', 'ready', '1']), patch.object(
                forge.urllib.request, 'build_opener') as build, contextlib.redirect_stderr(io.StringIO()) as err:
            build.return_value.open.side_effect = urllib.error.HTTPError(
                'https://forge.invalid', 403, 'secret', {}, io.BytesIO(b'secret'))
            self.assertEqual(forge.main(), 1)
            self.assertEqual(build.return_value.open.call_count, 1)
            self.assertIn('403', err.getvalue())
            self.assertNotIn('secret', err.getvalue())


if __name__ == '__main__':
    unittest.main()
