# ducktape bash completion — hand-written; checked against the clap tree.
# The node-bin drift guard (cli.rs tests::completion_files_match_the_clap_tree)
# fails the build if a verb token or flag here drifts from the grammar.
#
# install: source this file, or drop it in /etc/bash_completion.d/.

_ducktape() {
    local cur prev
    cur="${COMP_WORDS[COMP_CWORD]}"
    prev="${COMP_WORDS[COMP_CWORD-1]}"

    local families="node user account wallet gateway fs service agent module collab mcp help --help -h --version -V"

    local node_verbs="run key init invite admit join list status peers resident member work sandbox log-filter netstack help"
    local node_resident="accept remove"
    local node_member="promote remove leave status"
    local node_work="list admit revoke"
    local node_join="requests state"
    local node_netstack="swap"
    local node_flags="--config -n --network --sync-only --json --yes --out --dir --name --modules --genesis --listen --advertised --http --rpc --gateway --primary-coordinator --wireguard-listen --wireguard-advertised --invite-listen --block-time-ms --ttl-days --node --key --native --component --trust-node"

    local user_key="init restore unlock reveal status"
    local user_cred="add list remove grant revoke inspect seal"
    local user_verbs="key sign-gateway-route sign-frame sign-admin sign-caller cred help"
    local user_flags="--path --method --statement --out --key --node -n --network --node-key --publisher-node --account --route --name --json --host --remote --attest --pccs-url --snp-product --snp-vcek --vendor --measurement --credentials --cred-kind --token-stdin"
    local account_verbs="create show key login set-name set-profile help"
    local account_key="list approve add join remove"
    local account_flags="--node -n --network --key --auth-page --no-browser --name --eth --number --pubkey --scheme --passkey --ssh --label --ticket --avatar --bio"
    local wallet_verbs="new import list use help"
    local wallet_flags="--config --workspace -n --network --json"
    local gateway_verbs="bind unbind list help"
    local gateway_flags="--config --workspace -n --network --label --port --account"
    local fs_verbs="ls cat stat history diff checkout status commit pin unpin help"
    local fs_flags="-n --network --json --node --message --no-rebase --snapshot --limit --prefix --path --key --trust-node"
    local service_verbs="run list enable disable status help"
    local service_flags="--config --workspace -n --network --json --yes -y --enable --no-enable"
    # every service verb takes a KIND now, `list`/`status` included.
    local service_kinds="compute agent airlock"
    local agent_verbs="pty model-program sched install cancel reassign help"
    local agent_flags="-n --network --node --key --host-node --cred --cpu --mem --attempt"
    local module_verbs="pack update register status help"
    local module_flags="--index --view --assets --out --after --config -n --network --json"
    local collab_verbs="query key attach send ack help"
    local collab_flags="--target --node -n --network --key --trust-node --channel --participant --existing-only --device --expect --to --kind --message-id --credential --seq --ttl-secs --state --reason"

    if [ "$COMP_CWORD" -eq 1 ]; then
        COMPREPLY=( $(compgen -W "$families" -- "$cur") )
        return
    fi

    case "${COMP_WORDS[1]}" in
        node)
            case "${COMP_WORDS[2]}" in
                resident) COMPREPLY=( $(compgen -W "$node_resident $node_flags" -- "$cur") ) ;;
                member)   COMPREPLY=( $(compgen -W "$node_member $node_flags" -- "$cur") ) ;;
                work)     COMPREPLY=( $(compgen -W "$node_work $node_flags" -- "$cur") ) ;;
                join)     COMPREPLY=( $(compgen -W "$node_join $node_flags" -- "$cur") ) ;;
                netstack) COMPREPLY=( $(compgen -W "$node_netstack $node_flags" -- "$cur") ) ;;
                run|key|init|invite|admit|list|status|peers|sandbox|log-filter)
                          COMPREPLY=( $(compgen -W "$node_flags" -- "$cur") ) ;;
                *)        COMPREPLY=( $(compgen -W "$node_verbs" -- "$cur") ) ;;
            esac
            ;;
        user)
            case "${COMP_WORDS[2]}" in
                key)  COMPREPLY=( $(compgen -W "$user_key $user_flags" -- "$cur") ) ;;
                cred) COMPREPLY=( $(compgen -W "$user_cred $user_flags" -- "$cur") ) ;;
                *)    COMPREPLY=( $(compgen -W "$user_verbs $user_flags" -- "$cur") ) ;;
            esac
            ;;
        account)
            case "${COMP_WORDS[2]}" in
                key) COMPREPLY=( $(compgen -W "$account_key $account_flags" -- "$cur") ) ;;
                *)   COMPREPLY=( $(compgen -W "$account_verbs $account_flags" -- "$cur") ) ;;
            esac
            ;;
        wallet)  COMPREPLY=( $(compgen -W "$wallet_verbs $wallet_flags" -- "$cur") ) ;;
        gateway) COMPREPLY=( $(compgen -W "$gateway_verbs $gateway_flags" -- "$cur") ) ;;
        fs)      COMPREPLY=( $(compgen -W "$fs_verbs $fs_flags" -- "$cur") ) ;;
        service)
            case "${COMP_WORDS[2]}" in
                run|list|enable|disable|status)
                          COMPREPLY=( $(compgen -W "$service_kinds $service_flags" -- "$cur") ) ;;
                *)        COMPREPLY=( $(compgen -W "$service_verbs $service_flags" -- "$cur") ) ;;
            esac
            ;;
        agent)   COMPREPLY=( $(compgen -W "$agent_verbs $agent_flags" -- "$cur") ) ;;
        module)  COMPREPLY=( $(compgen -W "$module_verbs $module_flags" -- "$cur") ) ;;
        collab)  COMPREPLY=( $(compgen -W "$collab_verbs $collab_flags" -- "$cur") ) ;;
    esac
}

complete -F _ducktape ducktape
