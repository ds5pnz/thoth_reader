# Release notes

Put the text of a release in `<tag>.md` (for example `v1.2.0.md`) before pushing the tag.
The release workflow (`.github/workflows/release.yml`) uses it as the release description;
without the file it falls back to the list of commits since the previous tag.

The build is not signed with an Apple Developer ID, so every release description should keep
the install note:

```bash
xattr -dr com.apple.quarantine "/Applications/Thoth Reader.app"
```

The Windows installer is not code-signed either: Windows SmartScreen shows a warning
("Windows protected your PC") that is passed with *More info → Run anyway*.
