-- Variants are found by their template, an overlay field kept in the item's ext column: an
-- expression index keeps template checks and the variant matrix cheap on large item masters.
create index if not exists e_item_stk_variant_of on e_item (tenant_id, (ext->>'stk_variant_of'));
