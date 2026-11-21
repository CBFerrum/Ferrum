-- No agent key made before agents may sign in goes live without its owner making it again at a screen (AG-1-2-fix 5):
-- a build without agents now refuses to make one, and every one made so far, in any build, is revoked here.
update api_key set revoked_at = now() where agent_id is not null and revoked_at is null;
