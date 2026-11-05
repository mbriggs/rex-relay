# Agent commit signing and push access

Claude sessions that commit in this repo stall on a 1Password biometric prompt when nobody is at
the Mac. This guide sets up two credentials that belong to the agents and nothing else:

- An ed25519 SSH signing key, stored in 1Password and registered on GitHub as a Signing Key. It
  signs commits. It can't authenticate to GitHub, so it can't push.
- A push credential limited to `mbriggs/rex-relay`: a fine-grained personal access token, or a
  write deploy key as the alternative.

Every command below runs on the Mac, from the root of a `rex-relay` checkout unless it says
otherwise. Repo-local git config lives in the shared `.git` directory, so worktrees created with
`git worktree add` pick it up too.

Names used throughout:

| Thing                 | Name                              |
| --------------------- | --------------------------------- |
| 1Password vault       | `Agents`, not Personal or Private |
| 1Password item        | `rex-relay-agent-signing`         |
| Signing key file      | `~/.ssh/rex-relay-agent-signing`  |
| Allowed signers file  | `~/.config/git/allowed_signers`   |
| Keychain item for PAT | `rex-relay-agent-push`            |

## 1. Generate the signing key in 1Password

Put the key in its own vault. The 1Password SSH agent offers keys from the Personal and Private
vaults by default, and keeping this one out of them stops the agent from intercepting signatures
in option B below.

In the app: create a vault named `Agents`, then New Item > SSH Key > Add Private Key > Generate a
New Key > Ed25519, and title it `rex-relay-agent-signing`.

Or with the 1Password CLI:

```sh
op vault create Agents
op item create --vault Agents --category ssh --title rex-relay-agent-signing \
  --ssh-generate-key ed25519
```

Save the public key next to where the private key will go:

```sh
mkdir -p ~/.ssh && chmod 700 ~/.ssh
op read "op://Agents/rex-relay-agent-signing/public key" > ~/.ssh/rex-relay-agent-signing.pub
```

## 2. Choose how the key reaches git

The 1Password agent can't sign with no human at all: it needs 1Password unlocked and at least one
approval. So there are two options.

**Option A, 1Password agent with remembered approval.** The private key never leaves 1Password.
In Settings > Developer > SSH Agent, set approvals to be asked once per application and remembered
until 1Password locks, and lengthen auto-lock under Settings > Security. The agent still asks
after every lock, restart, or app quit. An agent that commits at 3am after the Mac locked will
stall exactly as it does today. Labels in those settings move between 1Password releases, so check
them against the app.

Add the `Agents` vault to the agent. Creating `agent.toml` replaces the default vault list, so keep
the Private line:

```sh
mkdir -p ~/.config/1Password/ssh
cat >> ~/.config/1Password/ssh/agent.toml <<'EOF'
[[ssh-keys]]
vault = "Private"

[[ssh-keys]]
item = "rex-relay-agent-signing"
vault = "Agents"
EOF
```

If that file already existed, open it and check you didn't duplicate the Private entry.

**Option B, a key file for unattended use (recommended).** Export the private key to disk and let
`ssh-keygen` sign with it directly. 1Password stays the source of truth and the backup, and
nothing prompts. The cost: the key sits unencrypted in `~/.ssh`, and any process running as you
can sign commits that GitHub marks Verified. It still can't push or log in anywhere, and deleting
it from GitHub (section 6) ends its Verified status for new commits. That trade is the reason the
key exists at all, so this guide uses option B from here on and notes where A differs.

```sh
(umask 077 && op read "op://Agents/rex-relay-agent-signing/private key?ssh-format=openssh" \
  > ~/.ssh/rex-relay-agent-signing)
ls -l ~/.ssh/rex-relay-agent-signing   # expect -rw-------
```

## 3. Register the key on GitHub as a Signing Key

In the browser: Settings > SSH and GPG keys > New SSH key, Key type "Signing Key", title
`rex-relay agent signing`, and paste the contents of `~/.ssh/rex-relay-agent-signing.pub`.

Or with `gh`, which needs an extra scope once:

```sh
gh auth refresh -h github.com -s admin:ssh_signing_key
gh ssh-key add ~/.ssh/rex-relay-agent-signing.pub --type signing --title "rex-relay agent signing"
gh ssh-key list
```

Don't add it as an Authentication Key. Keeping it signing-only is what limits the damage if the
file leaks.

GitHub marks a commit Verified only when its committer email is a verified email on the account.
Check that `git config user.email` in this repo matches one.

## 4. Configure git for this repo

```sh
git config --local gpg.format ssh
git config --local commit.gpgsign true
git config --local tag.gpgsign true
git config --local user.signingkey "$HOME/.ssh/rex-relay-agent-signing"
git config --local gpg.ssh.program ssh-keygen
git config --local gpg.ssh.allowedSignersFile "$HOME/.config/git/allowed_signers"
```

`gpg.ssh.program ssh-keygen` overrides a global setting that points at 1Password's `op-ssh-sign`.
`user.signingkey` names the private key file, so `ssh-keygen` signs without asking any agent.

For option A, use these two lines in place of the matching ones above:

```sh
git config --local user.signingkey "$HOME/.ssh/rex-relay-agent-signing.pub"
git config --local gpg.ssh.program /Applications/1Password.app/Contents/MacOS/op-ssh-sign
```

This config signs every commit made in the repo with the agent key, yours included. To sign one
commit with your own key, pass `-c user.signingkey=... -c gpg.ssh.program=...` to that
`git commit`.

Add the key to the allowed signers file so `git log --show-signature` can check signatures locally:

```sh
mkdir -p ~/.config/git
echo "$(git config user.email) namespaces=\"git\" $(cut -d' ' -f1,2 ~/.ssh/rex-relay-agent-signing.pub)" \
  >> ~/.config/git/allowed_signers
```

## 5. Push credential

The remote probably uses SSH today, which means pushes go through your 1Password authentication
key and prompt like commits do. Point pushes at HTTPS and give them a token that can only touch
this repo.

### Fine-grained personal access token (recommended)

Create it at Settings > Developer settings > Personal access tokens > Fine-grained tokens >
Generate new token:

- Token name: `rex-relay agent push`
- Expiration: 90 days. GitHub emails before it expires.
- Resource owner: `mbriggs`
- Repository access: Only select repositories > `mbriggs/rex-relay`
- Repository permissions: Contents > Read and write. Metadata > Read-only is added for you. Leave
  everything else at No access.

Copy the token into a new 1Password item in the `Agents` vault, then into the login keychain.
`security` prompts for the token; paste it there rather than on the command line:

```sh
security add-generic-password -a "$USER" -s rex-relay-agent-push -w
```

Point pushes at HTTPS and make this repo read the token from the keychain. The empty `helper`
line clears helpers set globally, such as `gh auth git-credential`, so `gh`'s broader token never
answers for this repo:

```sh
git remote set-url --push origin https://github.com/mbriggs/rex-relay.git
git config --local credential.https://github.com.helper ''
git config --local --add credential.https://github.com.helper \
  '!f() { test "$1" = get || exit 0; echo username=mbriggs; echo "password=$(security find-generic-password -s rex-relay-agent-push -w)"; }; f'
```

Fetches keep the SSH URL and can still prompt for your 1Password authentication key. If that gets
in the way, move fetches to HTTPS too. They'll use the same token:

```sh
git remote set-url origin https://github.com/mbriggs/rex-relay.git
```

The token covers git pushes only. `gh pr create` uses `gh`'s own login.

### Alternative: write deploy key

A deploy key is an SSH key attached to the repo. It can't expire, which is the main reason the
token comes first. Use it if you'd rather keep pushes on SSH.

```sh
ssh-keygen -t ed25519 -N '' -C "rex-relay agent deploy key" -f ~/.ssh/rex-relay-agent-deploy
gh repo deploy-key add ~/.ssh/rex-relay-agent-deploy.pub -R mbriggs/rex-relay \
  --title "rex-relay agent push" --allow-write
git config --local core.sshCommand \
  "ssh -i $HOME/.ssh/rex-relay-agent-deploy -o IdentitiesOnly=yes -o IdentityAgent=none"
git remote set-url --push origin git@github.com:mbriggs/rex-relay.git
```

`IdentityAgent=none` stops ssh from asking 1Password for a key first. Keep a copy of the private
key in the `Agents` vault if you want it backed up. Skip the credential helper lines above if you
use this.

## 6. Verify

Make a signed commit on a throwaway branch and check it locally:

```sh
git switch -c agent-signing-check
git commit --allow-empty -m "Check agent commit signing"
git log --show-signature -1
```

Expect a line like:

```text
Good "git" signature for you@example.com with ED25519 key SHA256:...
```

The fingerprint should match `ssh-keygen -lf ~/.ssh/rex-relay-agent-signing.pub`. "No principal
matched" means the email in `allowed_signers` doesn't match the committer email.

Push it with the new credential, then check GitHub:

```sh
git push -u origin agent-signing-check
gh api repos/mbriggs/rex-relay/commits/agent-signing-check --jq .commit.verification
```

Expect `"verified": true` and `"reason": "valid"`. The commit page on GitHub shows a Verified
badge. If neither the commit nor the push prompted, the setup works unattended. The token's page
under Fine-grained tokens shows a fresh "Last used" time if the push went through it.

Clean up:

```sh
git switch main
git branch -D agent-signing-check
git push origin --delete agent-signing-check
```

## 7. Rotate and revoke

### Signing key

Revoke when the key file may have leaked:

```sh
gh ssh-key list                 # find the ID of "rex-relay agent signing"
gh ssh-key delete <id>
rm ~/.ssh/rex-relay-agent-signing
```

Then delete the 1Password item and remove its line from `~/.config/git/allowed_signers`. GitHub
keeps the Verified badge on commits it verified before you deleted the key, and new commits signed
with it show Unverified.

Rotate on a schedule or after a revoke:

1. Generate a new key in 1Password (section 1) with a new title, such as
   `rex-relay-agent-signing-2027`.
2. Export it (section 2) and register it on GitHub (section 3).
3. Point `user.signingkey` at the new file and add its line to `allowed_signers`.
4. Run the checks in section 6.
5. Delete the old key from GitHub and disk. Keep its `allowed_signers` line so old commits still
   verify locally, unless you're rotating because it leaked.

### Personal access token

Revoke at Settings > Developer settings > Personal access tokens > Fine-grained tokens >
`rex-relay agent push` > Delete. Then:

```sh
security delete-generic-password -s rex-relay-agent-push
```

Rotate before expiry from the same page with Regenerate token, then replace the keychain entry
(`-U` updates in place) and the copy in 1Password:

```sh
security add-generic-password -U -a "$USER" -s rex-relay-agent-push -w
```

Push a throwaway branch (section 6) to confirm.

### Deploy key

```sh
gh repo deploy-key list -R mbriggs/rex-relay
gh repo deploy-key delete <id> -R mbriggs/rex-relay
rm ~/.ssh/rex-relay-agent-deploy ~/.ssh/rex-relay-agent-deploy.pub
```

To rotate, generate and add a new key as in section 5, push a test branch, then delete the old
one. Deploy keys never expire, so put a rotation date in your calendar.
