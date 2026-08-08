import os

with open('src/keys.rs', 'r') as f:
    lines = f.readlines()

def find_idx(substr):
    for i, line in enumerate(lines):
        if substr in line:
            return i
    return -1

account_start = find_idx("pub(crate) const DUPLICATE_SOFTWARE_ACCOUNT_MESSAGE")
keys_resume1 = find_idx("pub fn generate_mnemonic() -> String")
account_resume1 = find_idx("pub fn ensure_db_initialized(")
keys_resume2 = find_idx("pub fn parse_account_uuid")
account_resume2 = find_idx("fn resolve_account_id(")
keys_resume3 = find_idx("fn shielded_address_request()")
account_resume3 = find_idx("pub fn wallet_exists(")
tests_start = find_idx("#[cfg(test)]")
structs_start = find_idx("// ======================== Public API Structs ========================")

# Where to split tests
test_add_account = find_idx("fn test_create_wallet_and_get_address()")
test_end_account = find_idx("fn test_create_testnet_wallet()")

imports = lines[0:account_start]

account_lines = []
account_lines.extend(imports)
account_lines.append("use crate::keys::*;\n\n")

# account sections
account_lines.extend(lines[account_start:keys_resume1])
account_lines.extend(lines[account_resume1:keys_resume2])
account_lines.extend(lines[account_resume2:keys_resume3])
account_lines.extend(lines[account_resume3:tests_start])

# account tests
account_lines.append("#[cfg(test)]\nmod tests {\n    use super::*;\n\n")
account_lines.extend(lines[test_add_account:test_end_account])
account_lines.append("}\n")

keys_lines = []
keys_lines.extend(imports)
keys_lines.append("use crate::account::*;\n\n")

# keys sections
keys_lines.extend(lines[keys_resume1:account_resume1])
keys_lines.extend(lines[keys_resume2:account_resume2])
keys_lines.extend(lines[keys_resume3:account_resume3])

# keys tests (start to add_account, and testnet_wallet to structs)
keys_lines.extend(lines[tests_start:test_add_account])
keys_lines.extend(lines[test_end_account:structs_start])
# wait, there's a trailing '}' from the test module in keys.rs that is right before structs_start
# let's be careful. The structs_start is out of the test mod.
# test_end_account is "    fn test_create_testnet_wallet() {"
# structs_start has a '}' before it. Let's just use lines[test_end_account:structs_start]
# wait, tests_start is #[cfg(test)]. It includes `mod tests {`

keys_lines.extend(lines[structs_start:])

with open('src/account.rs', 'w') as f:
    f.writelines(account_lines)

# use r+ and truncate to preserve inode
with open('src/keys.rs', 'r+') as f:
    f.seek(0)
    f.writelines(keys_lines)
    f.truncate()
