# Product

<!-- impeccable:product-schema 1 -->

## Platform

macos

## Stack

SwiftUI, macOS 14+, Swift Package (app target + a stand-in agent for development), bundled into a regular `.app` by a script. Liquid Glass where macOS 26 gives it for free (sidebar, toolbar), never as decoration.

## Users

El dueño de una instalación casera de Fogar (hoy, una sola persona: Sito), en su Mac, en casa o fuera por Tailscale. Abre la app para saber si su Mac está unido a Fogar, para iniciar sesión la primera vez con su usuario y su contraseña de Fogar y, rara vez, para cerrarla. La mira de reojo desde la barra de menús.

## Product Purpose

Fogar Desktop es el programa de escritorio de Fogar: un agente Rust siempre vivo más esta app de compañía. La app enseña lo que el agente sabe (estado del enlace con el gateway, qué gateway, cómo se autenticó la app ante el agente) y hace lo poco que el agente permite: buscar gateways, iniciar sesión, cerrar sesión. Tiene éxito cuando el dueño inicia sesión en su Mac sin abrir la terminal y, después, ve de un vistazo «Conectado a green-house».

## Positioning

Es la cara en el Mac del mismo asistente casero que el dashboard del gateway y la pantalla de Fogar (Fogar TV): mismo lenguaje, misma regla de decir la verdad del estado, pero con las costumbres de una app de Mac (Dock, barra de menús, ⌘, , teclado).

## Operating Context

- La app habla con el agente por un socket Unix local (`~/Library/Application Support/com.fogar.agent/run/agent.sock`), tramas de 4 bytes + JSON, protocolo IPC v3. Si el agente no corre, la app lo dice.
- Pantallas: Estado, Iniciar sesión (gateways encontrados o dirección a mano + usuario y contraseña), Ajustes (cerrar sesión). Vista rápida en la barra de menús.
- Los slices siguientes añaden sesiones, chat, escucha y monitorización sobre la misma estructura.
- Sin cuenta de Apple Developer de pago: firma ad hoc o Personal Team; bundle id `com.fogar.desktop` (GUESSED).

## Capabilities and Constraints

- Solo enseña lo que el agente sabe hoy (`status`/`event`: fase, gateway, sesiones, turnos, modo de arranque, comprobación del par). Nada de secciones de relleno.
- La clave del cliente nunca llega a la app. La contraseña cruza la IPC una vez, no se guarda y solo viaja al gateway cifrada (https o Tailscale).
- Iniciar y cerrar sesión solo los acepta el agente si la app la firmó el mismo firmante; si no, `refused`.
- «Abrir al iniciar sesión» solo si es real (SMAppService); el empaquetado llega en el S7.
- Coste en reposo: suscripción, sin sondeo; la app cerrada no despierta al agente.

## Brand Commitments

- Nombre: Fogar (producto: Fogar Desktop, nombre abierto). Textos solo en español, tono cercano y directo.
- Sistema de fogar-brand: paleta «Lousa con humo de monte», Unbounded (solo titula) + Figtree (todo lo demás, cifras tabulares), la línea viva como movimiento de marca; brasa < 3 % de la pantalla en producto.
- Oscuro primero. Por decisión del dueño (2026-11-14) la app sigue al sistema: en modo claro usa una variante clara derivada (propuesta, no votada en fogar-brand).

## Evidence on Hand

Ninguna: no hay testimonios, cifras ni capturas de usuarios. No inventarlos. Los datos de ejemplo del agente sustituto son sintéticos.

## Product Principles

- Decir la verdad del estado: si el agente no corre o el enlace cae, se ve.
- Lo de cada día va rápido; con teclado nada espera a una animación.
- Menos pantalla: nada que el agente no sepa todavía.
- Las credenciales no se enseñan ni se registran más de lo imprescindible.

## Accessibility & Inclusion

WCAG AA en oscuro y en claro. Siempre se respeta «reducir movimiento». Todo funciona con teclado y VoiceOver.
