# A browser on a running server, 9 November 2026: what an update changes under customizations and waiting records (FEO-056)

The walk server was a debug build of the server crate without its compiled-in apps
(`--no-default-features`), from this change's branch on Ferrum main 529ee29. Its interface was built from
Ferrum-Frontend's `feo/customization-conflicts` branch on main a501ad3. The database was the walk's own
(`ferrum_feo_walk`), on port 5193, with the development defaults for apps: uploads reach the cluster,
and unsigned ones are allowed. The administrator of workspace `walk` was signed in from an earlier walk.

The app was Walk Kit, made for the walk as a metadata app:
- a Device whose serial has no label of its own;
- a Claim whose workflow goes Draft → Submitted → Approved.

1.1.0 labels the serial "Serial number", and whoever raised a claim no longer approves it. 1.2.0 labels the
serial "Device serial", and only a System Manager approves a claim, still not its raiser.

## What was walked

| Step | What happened |
|---|---|
| Install 1.0.0 | Uploaded to the cluster, then installed from Apps › Store through the consent dialog. |
| Customize | Customize › Device › Serial: label "Serial no.". The dialog holds the current label, so the first attempt appended to it ("SerialSerial no."). It was switched off from the list and done again. |
| A claim waiting | "Taxi to the depot" was raised and submitted, after the confirmation the workflow asks for. It waits in Submitted. |
| 1.1.0 uploaded to the cluster | This ran on the build before the notice below existed. The release became the cluster's at once. The upload's answer listed the waiting claim but not the customization, and nobody was told anything. That is what this change answers (findings). |
| 1.2.0 uploaded to the cluster | The administrator's bell: **Walk Kit 1.2.0 changed this workspace**, "One of your customizations replaces what it changed: keep it or switch it off under Customizations. 1 Claim waits in Submitted, whose ways out it changed: they go on under the new ones." The upload's own plan lists nothing more, since the release had already landed. |
| Customize | **Changed by an update** lists the conflict: Device, `Set serial.label = "Serial no."`, it replaced *Not set*, the update says *Device serial*, with **Keep mine** and **Switch off**. The buttons name what they act on for screen readers ("Keep mine: Set serial.label = …"). Keep mine: the section goes, no conflict is left, and the label stays "Serial no.". |
| The waiting claim | It is still in Submitted. Its raiser has no Approve button, and the API refuses the step: `403` "the creator of a document cannot approve it". This is the rule the notice described. |

## What it found

| Finding | Now |
|---|---|
| A release made available on the cluster changes every workspace's packages at once. This is the documented model (docs/operations.md, "Apps"): a workspace's own Update applies permissions, roles and seed data, not definitions. Before this change, only the workspace whose installation brought the release was told, and only of its customizations. | Fixed in this change. Every other workspace with the app is told what the release changed in it: customizations and waiting records. Where a workspace's plan brings the release (a store upgrade), the plan says the same before anyone agrees (`apps_api`). |
| The upload's answer read the workspace at the start of the request and the cluster after the release landed. It listed the waiting claim but not the customization. | Fixed in this change: the upload plans in the workspace as it is once the release has landed. |
| The section named the entity by its internal name (`WkDevice`), and called a label the package leaves unset "nothing". | Fixed before commit: the entity's label, and "Not set". |
| The property dialog starts with the current value, so typing appends to it. | Recorded: the value is ordinary editable text. |

## Not walked

The plan before consent, for a release that is not yet the cluster's, needs an app store, and the walk
server had none. It ran in `server/tests/apps_api.rs` with the test's store.
