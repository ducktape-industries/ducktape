// A blobs program for the kernel's tests, on the blobs contract: an account puts a bundle, a
// current validator's key receipts it, and a quorum of receipts marks it held.

#[cfg(target_arch = "wasm32")]
mod program {
    use abi::role::access::{Need, Resource};
    use abi::role::{self, validators};
    use abi::{Env, Origin, Principal, Refusal, Scan, reason};
    use blob::contract::{
        Bundle, Custody, Entry, Holding, NEXT, Op, Query as Ask, Reply, bundle_key, index_key,
        index_prefix, indexed_bundle, quorum,
    };
    use program::{Execute, Program, Query, Reads};

    struct Blobs;

    fn bundle(ctx: &impl Reads, n: u64) -> Result<Option<Bundle>, Refusal> {
        ctx.record(bundle_key(n))
    }

    fn put(ctx: &mut Execute, env: &Env, blobs: Vec<Entry>) -> Result<(), Refusal> {
        let Some(Principal::Account(putter)) = env.sender else {
            return Err(Refusal::new(reason::UNAUTHORIZED, "an account puts blobs"));
        };
        if blobs.is_empty() {
            return Err(Refusal::new(reason::INVALID_INPUT, "a put names a blob"));
        }
        let n: u64 = ctx.record(NEXT)?.unwrap_or(0);
        for entry in &blobs {
            ctx.set(index_key(&entry.id, n), Vec::new());
        }
        let bundle = Bundle {
            putter,
            height: env.height,
            blobs,
            custody: Custody::Receipted(Vec::new()),
        };
        ctx.put(bundle_key(n), &bundle);
        ctx.put(NEXT, &(n + 1));
        ctx.output(abi::encode(&n));
        Ok(())
    }

    fn receipt(ctx: &mut Execute, env: &Env, n: u64) -> Result<(), Refusal> {
        let Origin::External(key) = &env.origin else {
            return Err(Refusal::new(
                reason::UNAUTHORIZED,
                "a validator's key receipts",
            ));
        };
        let validators::Reply::Validators(seated) = ctx.ask(
            env.roles.bound(role::VALIDATORS)?.clone(),
            &validators::Query::Validators,
        )?
        else {
            return Err(Refusal::new(reason::UNEXPECTED_REPLY, "validators"));
        };
        if !seated.contains(key) {
            return Err(Refusal::new(
                reason::UNAUTHORIZED,
                "not a current validator",
            ));
        }
        let Some(mut bundle) = bundle(ctx, n)? else {
            return Err(Refusal::new(reason::NOT_FOUND, "no such bundle"));
        };
        let Custody::Receipted(mut keys) = bundle.custody else {
            return Err(Refusal::new(reason::WRONG_STATE, "the bundle is held"));
        };
        if keys.contains(key) {
            return Err(Refusal::new(reason::DUPLICATE, "already receipted"));
        }
        keys.push(key.clone());
        let counted = keys.iter().filter(|key| seated.contains(key)).count();
        bundle.custody = if counted >= quorum(seated.len()) {
            Custody::Held { since: env.height }
        } else {
            Custody::Receipted(keys)
        };
        ctx.put(bundle_key(n), &bundle);
        Ok(())
    }

    fn held(ctx: &Query, blob: abi::BlobId) -> Result<Option<Holding>, Refusal> {
        for entry in ctx.scan(Scan::prefix(index_prefix(&blob))) {
            let Some(n) = indexed_bundle(&entry.key, &blob) else {
                continue;
            };
            let Some(bundle) = bundle(ctx, n)? else {
                continue;
            };
            let Custody::Held { since } = bundle.custody else {
                continue;
            };
            let Some(named) = bundle.blobs.into_iter().find(|entry| entry.id == blob) else {
                continue;
            };
            return Ok(Some(Holding {
                bundle: n,
                len: named.len,
                kind: named.kind,
                since,
            }));
        }
        Ok(None)
    }

    impl Program for Blobs {
        fn execute(ctx: &mut Execute, env: &Env, payload: &[u8]) -> Result<(), Refusal> {
            match abi::decode(payload)? {
                Op::Put { blobs } => put(ctx, env, blobs),
                Op::Receipt { bundle } => receipt(ctx, env, bundle),
            }
        }

        fn access(_ctx: &mut Query, _env: &Env, _payload: &[u8]) -> Result<Vec<Need>, Refusal> {
            Ok(Vec::new())
        }

        fn resource(_ctx: &mut Query, _env: &Env, _key: &str) -> Result<Resource, Refusal> {
            Ok(Resource::default())
        }

        fn query(ctx: &mut Query, _env: &Env, request: &[u8]) -> Result<(), Refusal> {
            let reply = match abi::decode(request)? {
                Ask::Held { blob } => Reply::Held(held(ctx, blob)?),
                Ask::Bundle { n } => Reply::Bundle(bundle(ctx, n)?),
            };
            ctx.respond(abi::encode(&reply));
            Ok(())
        }
    }

    program::program!(Blobs);
}
