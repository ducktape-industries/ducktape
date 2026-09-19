#compdef ducktape
# ducktape zsh completion — hand-written; checked against the clap tree.
# The node-bin drift guard (cli.rs tests::completion_files_match_the_clap_tree)
# fails the build if a verb token or flag here drifts from the grammar.
#
# install: put this file on your $fpath as `_ducktape`, then `autoload -U _ducktape`.

_ducktape() {
    local families=(node user account wallet gateway fs service agent module collab mcp release forge help --help -h --version -V)

    local node_verbs=(run key init invite admit join list status qualify peers resident member work sandbox log-filter netstack help)
    local node_resident=(accept remove)
    local node_member=(promote remove leave status)
    local node_work=(list admit revoke)
    local node_join=(requests state)
    local node_netstack=(swap)
    local node_flags=(--config -n --network --sync-only --json --yes --out --dir --name --modules --genesis --listen --advertised --http --rpc --gateway --primary-coordinator --wireguard-listen --wireguard-advertised --invite-listen --block-time-ms --ttl-days --node --key --component --trust-node --compose-only)

    local user_key=(init restore unlock reveal status)
    local user_cred=(add list remove grant revoke inspect seal)
    local user_verbs=(codex claude apple-codesign key sign-gateway-route sign-frame sign-admin sign-caller cred help)
    local user_flags=(--snp-ask --snp-ark --team-id --p12-password-file --p12 --api-key --path --method --statement --out --key --node --config -n --network --node-key --publisher-node --head --body --name --json --host --remote --attest --pccs-url --snp-product --snp-vcek --vendor --measurement --credentials --cred-kind --token-stdin)
    local account_verbs=(create show key login set-name set-handle set-profile help)
    local account_key=(list approve add join remove)
    local account_flags=(--node --config -n --network --key --auth-page --no-browser --name --eth --number --pubkey --scheme --passkey --ssh --label --ticket --handle --avatar --bio)
    local wallet_verbs=(new import list use help)
    local wallet_flags=(--config --workspace -n --network --json)
    local gateway_verbs=(bind unbind list help)
    local gateway_flags=(--config --workspace -n --network --label --port --account --credential-file --trusted-loopback)
    local fs_verbs=(put ls cat stat history diff checkout status commit pin unpin help)
    local fs_flags=(-n --network --json --node --config --message --no-rebase --snapshot --limit --prefix --path --key --trust-node)
    local service_verbs=(run list enable disable status help)
    local service_flags=(--config --workspace -n --network --json --yes -y --enable --no-enable)
    # every service verb takes a KIND now, `list`/`status` included.
    local service_kinds=(compute agent airlock)
    local agent_verbs=(pty model-program sched install cancel reassign chief add status pause resume help)
    local agent_flags=(-n --network --node --config --key --host-node --account --route --cred --cpu --mem --attempt --package --worker --channel --retry-initialization --installed-by --yes -y)
    local module_verbs=(pack update register status help)
    local module_flags=(--index --view --assets --lanes --out --after --config -n --network --json)
    local release_verbs=(sign-bundle manifest sign verify schedule withdraw key set status help)
    local release_flags=(-n --unpack-into --node --network --config --credential --out --sequence --display --node-contract --notes-url --kind --archive --successor-key --successor-from --key --sig --pubkey --sha --at --lead --json --skip-preflight-i-know-the-wit-moved)
    local collab_verbs=(query key attach send ack help)
    local collab_flags=(--target --node --config -n --network --key --trust-node --channel --participant --existing-only --device --expect --to --kind --message-id --credential --seq --ttl-secs --state --reason)
    local forge_verbs=(setup publish help)
    local forge_flags=(--node --config -n --network --key)

    if (( CURRENT == 2 )); then
        compadd -- $families
        return
    fi

    case ${words[2]} in
        node)
            case ${words[3]} in
                resident) compadd -- $node_resident $node_flags ;;
                member)   compadd -- $node_member $node_flags ;;
                work)     compadd -- $node_work $node_flags ;;
                join)     compadd -- $node_join $node_flags ;;
                netstack) compadd -- $node_netstack $node_flags ;;
                run|key|init|invite|admit|list|status|peers|sandbox|log-filter) compadd -- $node_flags ;;
                *)        compadd -- $node_verbs ;;
            esac
            ;;
        user)
            case ${words[3]} in
                key)  compadd -- $user_key $user_flags ;;
                cred) compadd -- $user_cred $user_flags ;;
                *)    compadd -- $user_verbs $user_flags ;;
            esac
            ;;
        account)
            case ${words[3]} in
                key) compadd -- $account_key $account_flags ;;
                *)   compadd -- $account_verbs $account_flags ;;
            esac
            ;;
        wallet)  compadd -- $wallet_verbs $wallet_flags ;;
        gateway) compadd -- $gateway_verbs $gateway_flags ;;
        fs)      compadd -- $fs_verbs $fs_flags ;;
        service)
            case $words[3] in
                run|list|enable|disable|status) compadd -- $service_kinds $service_flags ;;
                *) compadd -- $service_verbs $service_flags ;;
            esac
            ;;
        agent)   compadd -- $agent_verbs $agent_flags ;;
        module)  compadd -- $module_verbs $module_flags ;;
        collab)  compadd -- $collab_verbs $collab_flags ;;
        release) compadd -- $release_verbs $release_flags ;;
        forge)   compadd -- $forge_verbs $forge_flags ;;
    esac
}

_ducktape "$@"
