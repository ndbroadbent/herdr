# herdr-pr-watch

Fills herdr's sidebar with each session's PR, CI, deploy queue state and Terraform claims. Herdr itself knows nothing
about GitHub: this script writes pane metadata tokens, and `[ui.sidebar.agents]` in `config.toml`
decides how they look. The script's docstring lists the tokens and how a session's PR is found.

```bash
contrib/pr-watch/herdr-pr-watch --dry-run   # print what it would set
contrib/pr-watch/herdr-pr-watch             # set it once
```

To run it every 15 seconds (one run costs 1 point of GitHub's 5000/hour GraphQL budget, however many
PRs are open, so that's about 5% of it):

```bash
cp contrib/pr-watch/com.ndbroadbent.herdr-pr-watch.plist ~/Library/LaunchAgents/
launchctl bootstrap "gui/$(id -u)" ~/Library/LaunchAgents/com.ndbroadbent.herdr-pr-watch.plist
```

To stop it:

```bash
launchctl bootout "gui/$(id -u)/com.ndbroadbent.herdr-pr-watch"
```

It needs `gh`, and a CircleCI token in `CIRCLE_TOKEN` or `~/.circleci/cli.yml` to link the CI status.
The deploy queue is tailnet only; off the tailnet the `stage` and `tf` tokens keep their last values. Its cursor
into each transcript and its pipeline cache live in `~/.cache/herdr-pr-watch/state.json`, and
deleting that file only costs one full rescan.

Tests: `python3 contrib/pr-watch/test_herdr_pr_watch.py`
