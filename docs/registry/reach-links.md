# Links app code reaches records by, reviewed

`scripts/reach_links.py` finds an app's code that lists or queries records as the system (`sys(`, `elevated(`,
`Ctx::system`, `system_for`) and filters them by a link to a mergeable entity: that link decides who reaches the
records (an invoice a portal shows by its customer), though no rule names it. A merge moving such a link is an
administrator's (docs/guides/05, R19), so the entity declares it in `reach_links`. Where a read by such a link decides
no one's reach (the code reads for the system's own work and shows the records to nobody), a row here says why, and
the script passes it.

| App | Entity.field | Why it decides no one's reach |
|---|---|---|
