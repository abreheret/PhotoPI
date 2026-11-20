# PhotoPI

*[English version](README.md)*

Un petit serveur de sauvegarde de photos et vidéos pour Raspberry Pi,
compatible avec l'**application mobile Immich**.

Le serveur officiel [Immich](https://immich.app) demande Postgres, Redis et un
service d'apprentissage automatique (6 Go de RAM ou plus). C'est trop pour un
Raspberry Pi 4 qui fait déjà tourner d'autres choses. PhotoPI est un seul
binaire Rust avec SQLite. Il n'implémente que la partie de l'API Immich dont
l'app mobile a besoin pour sauvegarder et parcourir les photos, et occupe
environ **30 Mo de RAM**.

PhotoPI n'est pas affilié au projet Immich.

## Ce qui fonctionne

- Connexion par email et mot de passe (plusieurs utilisateurs possibles).
- **Sauvegarde** depuis l'app, au premier plan et en arrière-plan. Les fichiers
  déjà présents sur le serveur sont ignorés : l'app compare les empreintes SHA-1.
- **Timeline** dans l'app, via le flux de synchronisation Immich.
- **Miniatures** et aperçus pour les photos et les vidéos (vidéos via ffmpeg),
  préparés en arrière-plan juste après chaque envoi.
- **Visionneuse plein écran**, téléchargement de l'original, lecture des vidéos
  (sans transcodage, requêtes HTTP Range gérées).
- Favoris, archive, description.
- **Corbeille** : une photo supprimée depuis l'app va dans la corbeille pendant
  30 jours et peut être restaurée. Ensuite, ou avec « supprimer
  définitivement », le fichier est effacé du disque.
- Métadonnées des photos : date et fuseau horaire, GPS, appareil, objectif,
  exposition (EXIF). Des vidéos : durée, taille, images/s, date, GPS (ffprobe).

Non pris en charge : albums, partage, partenaires, visages, recherche
intelligente, carte, souvenirs, tags, piles, OAuth, retouche. L'app masque la
plupart de ces écrans car le serveur les déclare désactivés ; les autres
répondent `501 Not supported by PhotoPI`.

Testé avec l'app **v3.4.2** (Android). PhotoPI se présente comme un serveur
3.4.2 pour que l'app utilise le protocole correspondant. Une autre version de
l'app peut attendre une autre API : voir [docs/API-SUBSET.md](docs/API-SUBSET.md).

## Rangement des fichiers

```
PHOTO_ROOT/<utilisateur>/<AAAA>/<MM>/<nom d'origine>
```

La date est celle de la prise de vue (EXIF), en heure locale. `<utilisateur>`
est la partie de l'email avant `@`. Un même nom dans le même mois reçoit un
suffixe `+1`, `+2`… Les fichiers ne sont jamais renommés ni convertis : le
dossier est une photothèque normale, à copier, parcourir ou sauvegarder avec
n'importe quel outil.

La base de données (SQLite) et le cache des miniatures restent sur la carte SD
(`~/.local/share/photopi`, `~/.cache/photopi`). Seules les photos vont sur le
disque.

## Installation sur le Raspberry Pi

Sur le Pi : un système 64 bits (aarch64), et `sudo apt install ffmpeg` pour les
miniatures et métadonnées des vidéos.

Depuis l'ordinateur (il faut Rust, `zig` et `cargo-zigbuild` pour la
compilation croisée) :

```sh
PI=pi@raspberrypi.local ./deploy/deploy.sh
```

Le script lance les tests, compile, copie le binaire dans `~/photopi/` sur le
Pi, puis installe et démarre le service systemd `photopi`. Vérifiez d'abord
l'unité `deploy/photopi.service` : `PHOTO_ROOT` doit pointer vers votre dossier
photos, qui doit exister. Le service ne démarre que si le disque de ce dossier
est monté : les photos ne peuvent pas atterrir sur la carte SD par erreur.

Créez ensuite votre compte sur le Pi. Le mot de passe est demandé deux fois, sans s'afficher :

```sh
ssh -t pi@raspberrypi.local '~/photopi/photopi user add vous@example.com "Votre Nom"'
```

Dans un script, envoyez plutôt le mot de passe sur l'entrée standard (une ligne ; les espaces sont conservés).

Dans l'app Immich, saisissez l'adresse du serveur `http://raspberrypi.local:2283`
(ou l'adresse IP du Pi), puis votre email et votre mot de passe.

### Réglages (variables d'environnement)

| Variable | Défaut | |
|---|---|---|
| `PHOTO_ROOT` | `/mnt/music/photo` | dossier des photos (doit exister) |
| `PHOTO_DB` | `~/.local/share/photopi/photopi.db` | base SQLite |
| `PHOTO_CACHE` | `~/.cache/photopi` | miniatures |
| `PHOTO_BIND` | `0.0.0.0:2283` | adresse d'écoute (2283 = port d'Immich) |
| `PHOTO_TRASH_DAYS` | `30` | jours avant de vider la corbeille |
| `RUST_LOG` | `info` | niveau de journalisation |

### Utilisateurs

```sh
~/photopi/photopi user add <email> <nom>   # le premier est administrateur
~/photopi/photopi user passwd <email>      # déconnecte aussi tous les appareils
~/photopi/photopi user list
```

Lancez-les avec le même `PHOTO_DB` que le service. La valeur par défaut convient
pour l'utilisateur `pi`.

## Exploitation

- Journaux : `journalctl -u photopi -f`
- État et mémoire : `systemctl status photopi`
- Sauvegardez la base (`photopi.db`) en plus des photos : elle contient les
  comptes, favoris, corbeille et descriptions. Les photos sont des fichiers
  ordinaires.

## Sécurité

PhotoPI parle en HTTP simple et est fait pour le réseau local. N'exposez pas le
port 2283 sur Internet. Pour un accès à distance, passez par un VPN (WireGuard,
Tailscale…) ou un reverse proxy HTTPS. Les mots de passe sont stockés en
hachage Argon2, les jetons d'accès en SHA-256.

## Développement

```sh
cargo test      # tests unitaires + un test de bout en bout des parcours de l'app
cargo clippy --all-targets
```

- [docs/API-SUBSET.md](docs/API-SUBSET.md) : les appels de l'API Immich faits
  par l'app et comment PhotoPI y répond.
- [docs/DEVLOG.md](docs/DEVLOG.md) : journal de développement.

## Licence

MIT, voir [LICENSE](LICENSE). Immich appartient à ses auteurs ; l'app Immich
est sous AGPL-3.0 et ne fait pas partie de ce dépôt.
