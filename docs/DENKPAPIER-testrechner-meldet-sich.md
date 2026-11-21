# Denkpapier: Der Testrechner meldet sich beim Mac (statt umgekehrt)

Stand 21.11.2026 · Anlass: Einrichtung des Windows-Testrechners (ThinkPad X1) · noch nicht gebaut,
nur auf Auftrag.

## Beobachtung (Frank)

> „Das haben wir nicht gut gemacht – das war mit NATS ein Kinderspiel.“

Mit NATS hat sich jede Bench **von sich aus** mit dem Mac verbunden (ausgehende Verbindung). Seit dem
Umbau auf HTTP (14.11.2026) ist es umgekehrt: `bench-mcp` auf dem Mac ruft die Bench auf dem
Testrechner an. Damit das geht, muss am Testrechner alles stimmen:

1. Haken „Im Netz erreichbar“ (`apiNetwork`) – wirkt erst nach Neustart; am X1 war er nach dem
   Setzen trotzdem `false` in `settings.json` (nicht gespeichert oder Speichern nicht bemerkt).
2. Windows-Netzprofil „Privat“ (nach frischer Installation „Öffentlich“) – Umstellen per Befehl
   braucht Administrator.
3. Firewall-Freigabe für Port 4320.
4. IP und 64-stelliger Token müssen von Hand auf den Mac in `~/.config/bench-mcp/targets.json`.
5. Claude Desktop neu starten.

Jeder Schritt einzeln ist klein, zusammen hat es über eine Stunde gekostet. Ausgehende Verbindungen
lässt Windows dagegen ohne Rückfrage zu (Profil „Öffentlich“ hin oder her) – deshalb war NATS einfach.

## Idee

Die Richtung wieder umdrehen, ohne NATS zurückzuholen:

- Die Bench auf dem Testrechner bekommt eine Einstellung **„Verbinden mit“** (Adresse der Mac-Bench,
  z. B. `http://192.168.8.214:4320`) und baut von sich aus eine dauerhafte Verbindung auf
  (WebSocket oder lang laufendes HTTP). Über diese Leitung schickt der Mac Aufrufe, der Testrechner
  antwortet.
- Die Mac-Bench führt die verbundenen Instanzen als Liste; `bench-mcp` fragt sie dort ab – keine
  `targets.json`, kein Token-Abtippen.
- **Kopplung** einmalig wie bei Bluetooth: Der Testrechner zeigt beim ersten Verbinden einen kurzen
  Code, der Mac fragt „Instanz *windows* koppeln? Code 4711“ – danach tauschen beide einen
  Schlüssel aus und merken ihn sich.
- Am Testrechner muss dann **kein Port offen** sein; nur der Mac lauscht (das tut er ohnehin).

## Kleiner Schritt vorab (falls der Umbau warten soll)

1. Haken „Im Netz erreichbar“ wirkt sofort (neu binden statt Neustart) und wird beim Umschalten
   gespeichert (kein separates „Speichern“ übersehen).
2. Unter Windows prüft die Bench beim Einschalten das Netzprofil und legt die Firewall-Regel mit einer
   einzigen Windows-Abfrage (UAC) selbst an.
3. Seite „Schnittstelle“ zeigt den fertigen `targets.json`-Eintrag (IP + Token) zum Kopieren und einen
   Knopf „Erreichbarkeit prüfen“.

## Offene Fragen

- Mac im Heimnetz hat eine feste Adresse? (Sonst mDNS-Name `Mac-mini-von-Frank.local`.)
- Verbindung verschlüsseln (Heimnetz: Schlüssel aus der Kopplung reicht für Signatur/Auth; TLS erst,
  wenn es über das Heimnetz hinausgeht).
