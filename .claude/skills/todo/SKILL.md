---
name: todo
description: Add a structured TODO entry to TODO.md based on conversation context or provided content
argument-hint: [description of the todo]
allowed-tools: Read, Edit, Write
---

Add a structured TODO entry to `TODO.md` at the project root.

## Determine content

If `$ARGUMENTS` is provided, use it as the TODO description.
Otherwise, infer the TODO from the current conversation context.

## Add the entry

Read `TODO.md` and append a new entry at the end using this format:

```
## <short title>

<description>

- **Added**: <today's date YYYY-MM-DD>
- **Status**: pending
```

If `TODO.md` does not exist, create it with a `# TODO` heading first.

Do not modify or remove existing entries.
