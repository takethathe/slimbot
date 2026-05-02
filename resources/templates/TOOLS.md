# Tool Usage Notes

Tool signatures are provided automatically via function calling.
This file documents non-obvious constraints and usage patterns.

## shell — Command Execution

- Commands have a 30 second timeout
- Returns stdout/stderr with exit code on failure
- Use for running programs, git operations, builds, and system queries

## file_reader — File Reading

- Read files relative to the workspace directory
- 50,000 character read limit
- Use to inspect file contents before making changes

## file_writer — File Writing

- Write content to a file, creating parent directories as needed
- Overwrites existing files — use with caution

## file_editor — Search and Replace

- Perform precise search-and-replace in a file
- The search string must match exactly once
- Read the file first to confirm the exact text to replace

## list_dir — Directory Listing

- List directory contents with type indicators (`[D]` directory, `[F]` file, `[L]` symlink)

## make_dir — Directory Creation

- Create directories recursively (like `mkdir -p`)

## Best Practices

- Read files before editing them
- Use `shell` for git operations, `file_editor` for code changes
- Verify changes with `cargo check` or equivalent before claiming success
- All paths are relative to the workspace directory
