# Expected output — recorded from this implementation

These files are **authored**, not captured. They are what `herdr-tower` prints
today, written down so that a change to it shows up as a diff in a pull request
rather than as a surprise on somebody's screen.

They may be rewritten, but only deliberately and visibly: a diff in one of them
means somebody changed the output.

## Rewriting them

```
UPDATE_EXPECTED=1 cargo test
```

Every file here is rewritten from the code, and **the diff is the point**. Read
it. A line that moved because a column was miscounted looks exactly like a line
that moved because somebody meant it to, and the only thing that tells them
apart is a person looking at the change before it lands.

Running this to make a red test green, without reading what changed, is the one
way to make these files worthless.

## What they pin

- the wording itself, line for line
- column widths and alignment, where a CJK-aware width bug tore the table apart
- row ordering, including the attention queue that put a five-hour block above a
  ninety-second one
- truncation, and which budget the ellipsis is charged to
- the JSON shape: field order, escaping, the trailing newline
- that `herdr-tower info` and the tower's detail card render the same card
- the remote MCP server's tool contract (`mcp-tools.json`): names, schemas,
  descriptions and hints, which are what a model reads before calling anything
- its prompt list (`mcp-prompts.json`): the ready-made requests and their
  arguments, which a client shows its user

## The fixture and the clock

The inputs live here too — `fixture/` (the record store), and the pinned clocks
`NOW`, `card.NOW` and `info.NOW`. Those are *inputs*, not expectations:
`UPDATE_EXPECTED=1` never rewrites them.
