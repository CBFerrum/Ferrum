# Flows

A flow is logic drawn as steps and wires: when a number stays high, raise an alarm; when a device goes
quiet, tell someone; when a battery is low, ask for a charge. It runs at the gateway, so it goes on
working with the link down, and what it remembers survives a restart.

## What a flow cannot do

Node-RED and ThingsBoard's rule chains do this and more, and a plant floor is why this does less.

- **There is no code.** Every step is one of the kinds below. A flow can be read and reviewed, and
  nothing in one can do what its steps do not say.
- **It cannot loop.** A flow with a cycle is refused, and one message's work is bounded, so a bad flow
  costs a little time and cannot hang a gateway.
- **It cannot move anything.** The strongest thing a flow can do is ask for a command, which goes
  through the same ledger as a person's: the device must be commissioned for it, the site must hold
  authority, and it runs only under an automation envelope someone approved, or when a person says.
  With the link down such a request waits, and if it has waited longer than its `expires`, or its flow
  has since been stopped, it is not issued, and the device's history says why.
- **It is not a safety function.** A flow runs on a general-purpose computer in a process that can
  stop. Nothing that protects a person may depend on one. Interlocks, emergency stops and collision
  avoidance stay with the equipment and its safety system.

## Steps

**When**: a number, a state change, an event, an attribute, a timer. A device can be a code, the start
of one and a star (`PRESS-*`), or empty for any. Each device is watched on its own: a hold on `PRESS-1`
says nothing about `PRESS-2`.

**Decide**: compare, in range, must last, gone quiet, only changes, not too often, wait, count, average
over time, calculate, combine, all true, remember, translate, choose by value, replace, take a part.

*Must last* has two inputs, and something must be wired to `reset`, or it holds for ever. *Combine* and
*all true* wait on inputs named by their wires, and can ignore values older than `fresh`, because a
stale reading is not a reading. Formulas use `+ - * /`, `round`, `abs`, `min`, `max` and `coalesce`,
with inputs in brackets: `[value] * 1.8 + 32`, in exact decimals. No answer is no message.

**Do**: raise an alarm, tell someone, ask for a command, record an event, record a number, set an
attribute, send elsewhere. An alarm must have something wired to `clear`. *Tell someone* and *ask for
a command* happen at most once per `every`, a minute unless you say, however often they are reached,
so a chatty sensor cannot flood people or the ledger.

## From a drawing to running

| Stage | What happens | Who |
|---|---|---|
| Draft | Can have faults, and reaches nothing | Edge Engineer |
| Simulated | A script is walked through it, by the code a gateway runs. It must do something | Edge Engineer |
| In review | It cannot change while someone is reading it | |
| Approved | By someone else, whatever roles the author also holds | Edge Reviewer |
| Running | At one site, which is how a change is tried, or at all of them | Edge Reviewer |

A new version needs simulating again: a simulation of something else proves nothing. Choosing *Change
it* makes a draft and leaves what is running alone, until the new one has been through the same.
Stopping is never a question of review: anyone who may work on flows may stop one. Each step is in the
audit trail.

## At the gateway

Flows arrive with the rest of a gateway's configuration, which takes `managed: true` in `site.yaml`.
A flow is fed everything a device reports that passed its quality limits, and the clock runs timers,
holds, silences and delays. What a flow decides is written to the journal before it counts, and
reaches the platform when the link allows.

What flows remember is written down every 15 seconds. After a crash a hold or window may have lost up
to that much, and an effect from those seconds may happen again: an alarm is idempotent, and a
message may repeat. A new revision starts with nothing remembered, because its steps may mean
something else now.

Flows run only on gateways. A device with no gateway, posting directly to the platform, has rules and
flows run for it by nothing yet ([gaps](references/edge.md)).
