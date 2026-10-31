update:
    git pull --ff-only
    nix build --extra-experimental-features 'nix-command flakes' --out-link /opt/endgame/current .#endgame
    install -m 0644 deploy/endgame.service /etc/systemd/system/endgame.service
    systemctl daemon-reload
    systemctl restart endgame.service
