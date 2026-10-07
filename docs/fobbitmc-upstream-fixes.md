# Upstream fixes used by the preservation branch

The successful villager trade callback fix is adapted from AdmerPRO's
[Pumpkin pull request #3765](https://github.com/Pumpkin-MC/Pumpkin/pull/3765),
reviewed at commit `4b74d1807acebc93c1ee1a07c07dca8bc9f40164`.

Only its gameplay change is used: send refreshed offers without acquiring the
merchant screen lock already held by the callback. The separate restock helper
still synchronizes the handler. This branch retains its Minecraft 26.3 packet
handling, including preservation of the incoming disabled flag.
