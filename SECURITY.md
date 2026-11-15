# Security

## Reporting a vulnerability

Report a vulnerability in private. Do not open a public issue for it, and do not post the details in
a public channel.

- **Discord:** contact the team in the [Zenex Discord](https://discord.gg/HJ4JPEtYcR).
- **Email:** admin@zenithprotocols.com

Include the affected contract and function, the steps or a test that reproduces the problem, and the
impact you expect. We confirm receipt and tell you whether the report is in scope.

## Bounty scope

Rewards are paid only for vulnerabilities that put user funds at risk: theft, permanent loss, or
permanent freezing of trader margin or vault deposits.

Other reports are welcome and are credited, but are not eligible for a reward.

For this scope:

- **Unrealised profit is not user funds.** A report whose impact is profit that a position could not
  realise is out of scope.
- **A freeze is permanent only if a contract upgrade cannot lift it.** The `market`, `oracle` and
  `factory` contracts are upgradeable.

The reward for a report in scope depends on the funds at risk and on the quality of the report.

## Known and accepted behaviour

These are design decisions. A report that restates one of them is out of scope.

- **Keepers are permissionless and profit-seeking.** A keeper fills any unlocked, fillable order at the
  first opportunity. An attack that needs an order to stay unfilled until a chosen moment is not valid.
- **A vault order that misses its `min_out` is cancelled.** It does not wait for a better price, so a
  vault order is not a limit order. The principal returns to the user and the execution fee goes to
  the keeper.
- **`factory.deploy` is permissionless.** Anyone can deploy a market and vault pair with itself as the
  admin. `is_deployed` proves only that this factory deployed the address.
- **The settlement token's issuer keeps its authorization control.** A payout that the issuer blocks
  parks as claimable credit for the user.
