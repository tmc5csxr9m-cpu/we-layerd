#!/usr/bin/env python3
"""Approve one monitor in GNOME's ScreenCast portal through AT-SPI.

Dependencies on Arch Linux: ``python-atspi`` and ``at-spi2-core``.

Run while the portal chooser is visible:

    ./scripts/gui-test/approve-screen-share.py

The adapter fails closed when it cannot identify exactly one monitor or one
share button. It never uses pointer coordinates or keyboard injection.
"""

from __future__ import annotations

import argparse
from collections import deque
from pathlib import Path
import sys
import time

import pyatspi


PORTAL_APPLICATION = "xdg-desktop-portal-gnome"
WINDOW_NAMES = {"Share Screen", "Screen Share", "共享屏幕"}
SHARE_BUTTON_PREFIXES = ("share", "分享", "共享")


def descendants(root):
    queue = deque([root])
    while queue:
        node = queue.popleft()
        yield node
        try:
            queue.extend(node.getChildAtIndex(index) for index in range(node.childCount))
        except Exception:
            continue


def role(node) -> str:
    try:
        return node.getRoleName()
    except Exception:
        return ""


def name(node) -> str:
    try:
        return node.name or ""
    except Exception:
        return ""


def has_state(node, state) -> bool:
    try:
        return node.getState().contains(state)
    except Exception:
        return False


def visible(node) -> bool:
    return has_state(node, pyatspi.STATE_SHOWING) and has_state(node, pyatspi.STATE_VISIBLE)


def click(node) -> None:
    try:
        actions = node.queryAction()
    except Exception as exc:
        raise RuntimeError(f"control has no AT-SPI action: {name(node)!r}") from exc

    matches = [index for index in range(actions.nActions) if actions.getName(index) == "click"]
    if len(matches) != 1 or not actions.doAction(matches[0]):
        raise RuntimeError(f"could not invoke unique click action: {name(node)!r}")


def portal_frames():
    desktop = pyatspi.Registry.getDesktop(0)
    applications = [app for app in desktop if name(app) == PORTAL_APPLICATION]
    if len(applications) != 1:
        return []
    return [
        node
        for node in descendants(applications[0])
        if role(node) == "frame" and name(node) in WINDOW_NAMES and visible(node)
    ]


def normalize_button_name(value: str) -> str:
    return value.replace("_", "").strip().casefold()


def share_buttons(frame):
    return [
        node
        for node in descendants(frame)
        if role(node) == "button"
        and visible(node)
        and normalize_button_name(name(node)).startswith(SHARE_BUTTON_PREFIXES)
    ]


def monitor_buttons(frame, requested_name: str | None):
    buttons = [
        node
        for node in descendants(frame)
        if role(node) == "toggle button" and visible(node)
    ]
    if requested_name is not None:
        buttons = [node for node in buttons if name(node) == requested_name]
    return buttons


def describe_tree(frame) -> str:
    lines = []
    for node in descendants(frame):
        node_name = name(node)
        node_role = role(node)
        if not node_name and node_role not in {"button", "toggle button"}:
            continue
        states = []
        for label, state in (
            ("visible", pyatspi.STATE_VISIBLE),
            ("showing", pyatspi.STATE_SHOWING),
            ("sensitive", pyatspi.STATE_SENSITIVE),
            ("pressed", pyatspi.STATE_PRESSED),
        ):
            if has_state(node, state):
                states.append(label)
        lines.append(f"{node_role}\t{node_name}\t{','.join(states)}")
    return "\n".join(lines) + "\n"


def wait_until(deadline: float, predicate, interval: float = 0.1):
    while time.monotonic() < deadline:
        value = predicate()
        if value:
            return value
        time.sleep(interval)
    return None


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--monitor", help="exact accessible monitor name; required if several exist")
    parser.add_argument("--timeout", type=float, default=15.0)
    parser.add_argument(
        "--failure-log",
        type=Path,
        default=Path("artifacts/gui/screen-share-portal-failure.txt"),
    )
    parser.add_argument("--dry-run", action="store_true", help="inspect only; do not approve")
    args = parser.parse_args()
    deadline = time.monotonic() + args.timeout

    try:
        frames = wait_until(deadline, portal_frames)
        if frames is None or len(frames) != 1:
            raise RuntimeError(f"expected one visible portal frame, found {len(frames or [])}")
        frame = frames[0]

        monitors = monitor_buttons(frame, args.monitor)
        buttons = share_buttons(frame)
        if len(monitors) != 1:
            raise RuntimeError(f"expected one monitor candidate, found {len(monitors)}")
        if len(buttons) != 1:
            raise RuntimeError(f"expected one share button, found {len(buttons)}")

        monitor = monitors[0]
        share = buttons[0]
        monitor_name = name(monitor)
        if args.dry_run:
            print(describe_tree(frame), end="")
            return 0

        if not has_state(monitor, pyatspi.STATE_PRESSED):
            click(monitor)

        stable_samples = 0
        while time.monotonic() < deadline and stable_samples < 3:
            ready = (
                has_state(monitor, pyatspi.STATE_PRESSED)
                and has_state(share, pyatspi.STATE_SENSITIVE)
                and visible(share)
            )
            stable_samples = stable_samples + 1 if ready else 0
            time.sleep(0.1)
        if stable_samples < 3:
            raise RuntimeError("monitor selection and share button did not become stably ready")

        click(share)
        closed = wait_until(deadline, lambda: not portal_frames())
        if closed is None:
            raise RuntimeError("portal chooser did not close after approval")

        print(f"approved monitor: {monitor_name}")
        return 0
    except Exception as exc:
        frames = portal_frames()
        args.failure_log.parent.mkdir(parents=True, exist_ok=True)
        evidence = describe_tree(frames[0]) if len(frames) == 1 else "portal frame unavailable\n"
        args.failure_log.write_text(f"error: {exc}\n{evidence}", encoding="utf-8")
        print(f"error: {exc}; evidence: {args.failure_log}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
