"""Tests for herdr-pr-watch's choice of PR. Run: python3 contrib/pr-watch/test_herdr_pr_watch.py"""

import importlib.machinery
import importlib.util
import json
import tempfile
import unittest
from pathlib import Path
from unittest import mock

loader = importlib.machinery.SourceFileLoader("watch", str(Path(__file__).with_name("herdr-pr-watch")))
spec = importlib.util.spec_from_loader("watch", loader)
watch = importlib.util.module_from_spec(spec)
loader.exec_module(watch)

SESSION = "11111111-2222-3333-4444-555555555555"
RESUME = f"claude --resume {SESSION}"
REPO = "DocSpring/docspring"


def url(number, repo=REPO):
    return f"https://github.com/{repo}/pull/{number}"


def queue_entry(number, joined_at=100, resume=RESUME):
    return {"pr_number": number, "pr_url": url(number), "resume": resume, "joined_at": joined_at, "phase": "staging_qa"}


def agent(**tokens):
    return {"pane_id": "w1:p1", "agent_session": {"value": SESSION}, "tokens": tokens}


class TranscriptTests(unittest.TestCase):
    def setUp(self):
        temp = tempfile.TemporaryDirectory()
        self.addCleanup(temp.cleanup)
        projects = Path(temp.name)
        (projects / "project").mkdir()
        self.path = projects / "project" / f"{SESSION}.jsonl"
        patcher = mock.patch.object(watch, "CLAUDE_PROJECTS", projects)
        patcher.start()
        self.addCleanup(patcher.stop)
        self.record = {}
        self.next_id = 0

    def run_command(self, command, output, is_error=False, timestamp="2026-11-06T00:00:10Z"):
        self.next_id += 1
        tool_id = f"toolu_{self.next_id}"
        use = {"type": "tool_use", "name": "Bash", "id": tool_id, "input": {"command": command}}
        result = {"type": "tool_result", "tool_use_id": tool_id, "content": output, "is_error": is_error}
        with self.path.open("a") as transcript:
            transcript.write(json.dumps({"message": {"content": [use]}}) + "\n")
            transcript.write(json.dumps({"timestamp": timestamp, "message": {"content": [result]}}) + "\n")
        watch.scan_transcript(SESSION, self.record)

    def test_a_new_pr_replaces_the_last_one_on_a_later_scan(self):
        self.run_command("gh pr create --fill", url(1), timestamp="2026-11-06T00:00:10Z")
        self.assertEqual(self.record["pr"], f"{REPO}#1")
        first_seen = self.record["pr_at"]

        self.run_command("gh pr create --fill", url(2), timestamp="2026-11-06T00:00:20Z")

        self.assertEqual(self.record["pr"], f"{REPO}#2")
        self.assertEqual(self.record["pr_at"], first_seen + 10)

    def test_a_create_that_prints_no_url_changes_nothing(self):
        self.run_command("gh pr create --fill", url(1))
        self.run_command("gh pr create --fill", "error: no commits between branches", is_error=True)
        self.assertEqual(self.record["pr"], f"{REPO}#1")

    def test_a_checkout_takes_over_from_the_created_pr(self):
        self.run_command("gh pr create --fill", url(1))
        self.run_command("cd ../wt && gh pr checkout 7 && git status", "Switched to branch 'x'")
        self.assertEqual(self.record["pr"], f"{REPO}#7")

    def test_a_failed_checkout_changes_nothing(self):
        self.run_command("gh pr create --fill", url(1))
        self.run_command("gh pr checkout 7", "no pull requests found", is_error=True)
        self.assertEqual(self.record["pr"], f"{REPO}#1")
        self.assertEqual(self.record["pending"], {})

    def test_commands_that_only_read_a_pr_change_nothing(self):
        self.run_command("gh pr create --fill", url(1))
        self.run_command("gh pr view 7 --json title", url(7))
        self.run_command("gh pr checks 7", "all passed")
        self.assertEqual(self.record["pr"], f"{REPO}#1")


class CheckoutTargetTests(unittest.TestCase):
    def test_targets(self):
        for command, expected in [
            ("gh pr checkout 7", f"{REPO}#7"),
            ("gh pr checkout 7 --detach", f"{REPO}#7"),
            ("gh pr checkout -b local 7", f"{REPO}#7"),
            ("gh pr checkout 7 -R Other/repo", "Other/repo#7"),
            ("gh pr checkout --repo=Other/repo 7", "Other/repo#7"),
            (f"gh pr checkout {url(7, 'Other/repo')}", "Other/repo#7"),
            ("gh pr checkout 1 && gh pr checkout 2", f"{REPO}#2"),
            ("gh pr checkout some-branch", None),
            ('grep "gh pr checkout 7" notes.md', None),
            ("gh pr view 7", None),
        ]:
            self.assertEqual(watch.checkout_target(command), expected, command)


class PickPrTests(unittest.TestCase):
    def pick(self, record, queue=None, **tokens):
        return watch.pick_pr(agent(**tokens), record, watch.index_queue(queue))

    def test_transcript_pr_when_the_queue_has_nothing(self):
        self.assertEqual(self.pick({"pr": f"{REPO}#1", "pr_at": 50}), f"{REPO}#1")

    def test_a_deploying_pr_beats_a_newer_one(self):
        record = {"pr": f"{REPO}#2", "pr_at": 200}
        self.assertEqual(self.pick(record, {"staging": queue_entry(1)}), f"{REPO}#1")
        self.assertEqual(self.pick(record, {"waiting": [queue_entry(1)]}), f"{REPO}#1")

    def test_an_on_hold_pr_stays_until_the_session_moves_on(self):
        queue = {"on_hold": [queue_entry(1, joined_at=100)]}
        self.assertEqual(self.pick({"pr": f"{REPO}#1", "pr_at": 50}, queue), f"{REPO}#1")
        self.assertEqual(self.pick({}, queue), f"{REPO}#1")
        self.assertEqual(self.pick({"pr": f"{REPO}#2", "pr_at": 200}, queue), f"{REPO}#2")

    def test_a_deploying_pr_beats_an_on_hold_one(self):
        queue = {"staging": queue_entry(1), "on_hold": [queue_entry(2, joined_at=300)]}
        self.assertEqual(self.pick({}, queue), f"{REPO}#1")

    def test_the_entry_furthest_along_wins_within_the_queue(self):
        queue = {"waiting": [queue_entry(1), queue_entry(2)]}
        self.assertEqual(self.pick({}, queue), f"{REPO}#1")

    def test_a_finished_entry_is_only_a_fallback(self):
        queue = {"done": [dict(queue_entry(1), finished_at=500)]}
        self.assertEqual(self.pick({"pr": f"{REPO}#2", "pr_at": 50}, queue), f"{REPO}#2")
        self.assertEqual(self.pick({}, queue), f"{REPO}#1")

    def test_pr_ref_beats_everything(self):
        queue = {"staging": queue_entry(1)}
        self.assertEqual(self.pick({"pr": f"{REPO}#2"}, queue, pr_ref="9"), f"{REPO}#9")

    def test_an_entry_with_no_pr_is_ignored(self):
        queue = {"waiting": [{"resume": RESUME}], "on_hold": [{"resume": "x"}]}
        self.assertEqual(watch.index_queue(queue).unfinished, {})
        self.assertIsNone(self.pick({}, queue))


class PrRefTests(unittest.TestCase):
    def test_pr_ref_holds_until_the_session_moves_to_another_pr(self):
        pane = agent(pr_ref="9")
        record = {"pr": f"{REPO}#1"}
        with mock.patch.object(watch, "report_tokens") as report:
            watch.drop_superseded_pr_ref(pane, record, dry_run=False)
            watch.drop_superseded_pr_ref(pane, record, dry_run=False)
            self.assertEqual(pane["tokens"]["pr_ref"], "9")
            report.assert_not_called()

            record["pr"] = f"{REPO}#2"
            watch.drop_superseded_pr_ref(pane, record, dry_run=False)

        self.assertNotIn("pr_ref", pane["tokens"])
        self.assertNotIn("pr_ref", record)
        report.assert_called_once_with("w1:p1", {}, ["pr_ref"])
        self.assertEqual(watch.pick_pr(pane, record, watch.index_queue(None)), f"{REPO}#2")

    def test_a_dry_run_ignores_a_superseded_pr_ref_without_clearing_it(self):
        pane = agent(pr_ref="9")
        record = {"pr": f"{REPO}#2", "pr_ref": ["9", f"{REPO}#1"]}
        with mock.patch.object(watch, "report_tokens") as report:
            watch.drop_superseded_pr_ref(pane, record, dry_run=True)
        self.assertNotIn("pr_ref", pane["tokens"])
        report.assert_not_called()

    def test_setting_a_different_pr_ref_starts_over(self):
        pane = agent(pr_ref="10")
        record = {"pr": f"{REPO}#2", "pr_ref": ["9", f"{REPO}#1"]}
        watch.drop_superseded_pr_ref(pane, record, dry_run=True)
        self.assertEqual(pane["tokens"]["pr_ref"], "10")
        self.assertEqual(record["pr_ref"], ["10", f"{REPO}#2"])


class StageTests(unittest.TestCase):
    def tokens(self, entry, queue_known=True, **current):
        pull = {"number": 2, "url": url(2), "state": "OPEN", "headRefOid": "abc"}
        return watch.desired_tokens(pull, entry, queue_known, current, {"pipelines": {"abc": [1, "gh/x"]}})

    def test_the_stage_comes_from_the_pr_shown_not_from_the_session(self):
        index = watch.index_queue({"on_hold": [queue_entry(1, resume=RESUME)]})
        self.assertNotIn("stage", self.tokens(index.unfinished.get(f"{REPO}#2")))
        self.assertEqual(self.tokens(index.unfinished.get(f"{REPO}#1"))["stage"], "staging qa")

    def test_off_the_tailnet_a_stage_only_survives_on_its_own_pr(self):
        self.assertEqual(self.tokens(None, queue_known=False, pr="#2", stage="on hold")["stage"], "on hold")
        self.assertNotIn("stage", self.tokens(None, queue_known=False, pr="#1", stage="on hold"))


if __name__ == "__main__":
    unittest.main()
