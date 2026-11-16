-- The app whose call a receipt is (crates/kernel/src/capability.rs, Kernel::call_tx): null for a person's, a process's,
-- an automation's or an agent's use of the gate.
alter table logic_effect add column caller text;
