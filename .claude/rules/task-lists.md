# Task Lists

Use task lists to divide complex work among agents. You become the manager; agents do the work.
Use agents to execute task lists (but not for simple lookups like reading a specific file).

## When to Use

- ANY non-trivial task (3+ steps or architectural decisions)
- If something goes sideways, STOP and re-plan immediately
- Use task lists for verification steps, not just building
- Write detailed specs upfront to reduce ambiguity

## Creating Task Lists

1. **Build a dependency tree** — identify parallel vs. blocking tasks
2. **Write self-contained descriptions** — agents have no inherited context; include everything they need
3. **Limit to 15 tasks** per list
4. **Instruct agents to use task lists** if their task also exceeds 25k tokens

## Recursive Task Lists

Agents can create their own task lists. Keep dividing until tasks are small enough to execute directly.
