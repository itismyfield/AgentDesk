# Discord text-command policy

Text commands use a registered command ID, an explicit text-surface status, and
the existing risk policy before the named handler is selected. This is separate
from the intake authentication/admission checks, which still apply.

| Risk | Existing permission contract |
| --- | --- |
| ReadOnly | Existing authorized users |
| Mutating | Existing authorized users; not globally owner-only |
| ShellOrToolGrant | Bot owner and the existing explicit-enable setting |
| CredentialSystem | Bot owner; no additional explicit-enable requirement |

The environment opt-in is read by production and supplied as a boolean to the
pure selection function. Existing named handlers retain their validation,
timeouts, authorization checks, and argument handling.

`!vc` remains an implemented Mutating command with its existing downstream voice
authorization check. `!cc` and `!skill` retain their shared handler and the
existing `clear`, `stop`, `pwd`, `health`, `status`, `inflight`, and `help` behavior.
The `clear` sub-alias gives guidance; it does not silently execute a clear.

The policy names `!sessions`, `!receipt`, `!usage`, `!model`, `!fast`, `!goals`,
`!deletesession`, and `!restart` have no implemented text handler and are denied
on the text surface. Their registered slash surfaces are unchanged.

Unknown and unavailable bang-prefixed text commands are consumed with a denial
attempt, including when preceded by a leading bot mention. They are not
reinterpreted as chat, queued provider instructions, or arbitrary process input.
The legacy raw-command entry point remains for internal compatibility but only
attempts a fixed denial; its generic execution capability has been removed.
Ordinary non-command chat and supported provider-native input are unchanged.

Regression evidence is intentionally limited: pure selection/risk tests check
decisions, the actual exhaustive enum match supplies compile-time coverage, and
bounded source tests check production wiring and preserved aliases. These tests
do not invoke the live dispatcher, high-risk handlers, Discord, or an executor,
and do not claim dynamic execution or reply-delivery coverage.
