text
sandhi keys add <provider> [--scheme api-key|bearer|oauth]   # prompts; never echoes raw key
sandhi keys list | mask | revoke <id>
sandhi keys share --user X --budget N --models m1,m2 [--expires ...] [--rate ...]
        # prints a virtual key + endpoint once
sandhi budget set <scope> <limit> [--window] [--policy block|warn]
sandhi budget list | usage <scope>
sandhi usage --by key|user|session|model [--since ...] [--format table|json]