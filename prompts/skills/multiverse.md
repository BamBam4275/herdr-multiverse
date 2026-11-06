# multiverse — skill playbook

Configures and safely improves this Multiverse setup itself — the plugin
configuration, not whatever project the fleet is dispatched against.

---

## Skill: multiverse

Inspect and improve this Multiverse setup itself — accounts, roles, chains,
skills, prompts.

### Read before proposing

- `multiverse check` — accounts, chains, and any unfilled placeholders.
- `multiverse accounts` — the seats and their availability.
- `config.example.toml` — the shape, with placeholders.
- Any project doc recording open questions or measured findings about the
  substrate, if this project keeps one. Proposing something already recorded
  there as undecided, or contradicting a measured finding with an argument
  rather than a new measurement, wastes everyone's time and suggests you did
  not look.

### The rules this system holds itself to

Apply them to your own proposals:

- **Never invent a model id, account name, or config-directory path.** Read it
  from the tool that owns it, or leave a `<<PLACEHOLDER: ...>>` and say what is
  needed. A plausible guess is worse than a marked gap, because a gap is visible.
- **The installed `herdr` binary wins.** Where a design document and the
  installed `herdr` disagree, the binary is right and the difference gets
  recorded rather than worked around.
- **Measure rather than assume.** This project has been wrong about its
  substrate before and found it by running things, not by reasoning about it.
  "It should propagate" is not a finding.
- **A placeholder blocks configuration, never code.** Code paths get written and
  tested while values are still missing.

### Real values never go in a tracked file

Account names, config directories, and the human's actual model choices live in
`config.local.toml`, which is gitignored. `config.example.toml` keeps
placeholders. If a decision needs to be durable, record the *decision and its
reasoning* in whatever document this project keeps for that — not the value
in the example.

### Changing account routing

Account selection is a correctness concern, not a convenience default. Two
rules:

- Selection is explicit and logged. Never inferred, never inherited from ambient
  state — that is why aliases resolving from account defaults are avoided.
- An account names its own CLI (`kind`), and a role says which model to run on
  each kind it can use. A chain may therefore span vendors — a cross-vendor
  review role needs that, to exclude the vendor that produced the work it is
  judging — and the loader checks the pairing rather than trusting it: a seat
  whose `kind` disagrees with the variable that selects it, or a chain naming
  a seat the role has no model for, is a config error.

### Propose, do not apply

Unless the brief's write scope says otherwise, produce the diff and the
reasoning. Configuration changes route real work to real accounts and spend real
quota; that decision belongs to the human.
