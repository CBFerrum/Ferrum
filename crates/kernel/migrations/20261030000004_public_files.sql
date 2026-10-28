-- Files published for anonymous viewing (storefront images, public documents). Only images and
-- PDFs, only when attached to a record the uploader can edit.
alter table file_object add column is_public boolean not null default false;
create index file_object_public on file_object (id) where is_public;
