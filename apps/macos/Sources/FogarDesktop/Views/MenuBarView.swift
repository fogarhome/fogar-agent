import AppKit
import FogarWire
import SwiftUI

/// The menu bar quick view: the state at a glance and a way into the window. Never animates.
struct MenuBarView: View {
    @Environment(AgentLink.self) private var link
    @Environment(Conversacion.self) private var conversacion
    @Environment(Navegacion.self) private var nav
    @Environment(\.openWindow) private var openWindow

    var body: some View {
        let t = Textos.titular(link.state, retirado: link.accesoRetirado)
        VStack(alignment: .leading, spacing: 16) {
            HStack(alignment: .center, spacing: 8) {
                LineaViva(pose: .rest, etiqueta: "Fogar", quieta: true).frame(width: 58)
                Text("Desktop").meta()
            }
            .accessibilityElement(children: .combine)
            .accessibilityLabel("Fogar Desktop")

            VStack(alignment: .leading, spacing: 6) {
                Estado(tono: t.tono, texto: t.titulo)
                    .font(Tipo.texto(15, peso: 600)).foregroundStyle(Color.hueso)
                Text(t.sub).font(Tipo.texto(13)).suave()
                    .fixedSize(horizontal: false, vertical: true)
                if let g = link.state.status?.gateway, link.state.status?.phase != .notPaired {
                    Text("\(Textos.cuenta(g)) · versión \(g.version)").meta()
                        .fixedSize(horizontal: false, vertical: true)
                }
            }

            if let modo = IconoModo(link.state, conversacion).texto {
                // Speaking or recording: say so, and offer the way to stop it right here.
                HStack(spacing: 10) {
                    Text(modo == "grabando" ? "Fogar te está escuchando" : "Fogar está hablando")
                        .font(Tipo.texto(14, peso: 550)).foregroundStyle(Color.hueso)
                    Spacer(minLength: 0)
                    if conversacion.cancelable {
                        Button { conversacion.cancelarAhora() } label: { Text("Cancelar") }
                            .buttonStyle(Pildora(pequena: true))
                    } else if conversacion.grabando {
                        // Like the box: stop, and the text waits in the box.
                        Button { conversacion.parar() } label: { Text("Parar") }
                            .buttonStyle(Pildora(pequena: true))
                    }
                }
            }

            VStack(alignment: .leading, spacing: 8) {
                Button { abrir(.conversacion) } label: { Text("Hablar con Fogar") }
                    .buttonStyle(Pildora(variante: .principal, pequena: true, ancho: true))
                Button { abrir(.estado) } label: { Text("Ver el estado") }
                    .buttonStyle(Pildora(pequena: true, ancho: true))
                switch link.state {
                case .live(let s) where s.phase == .notPaired:
                    Button { abrir(.iniciarSesion) } label: { Text("Iniciar sesión…") }
                        .buttonStyle(Pildora(pequena: true, ancho: true))
                case .notRunning, .refused, .noLiveStatus, .unintelligible, .noVerificado:
                    Button { link.retryNow() } label: { Text("Volver a llamar al agente") }
                        .buttonStyle(Pildora(pequena: true, ancho: true))
                default:
                    EmptyView()
                }
            }

            Rectangle().fill(Color.veta).frame(height: 1)
            Button("Salir de Fogar Desktop") { NSApp.terminate(nil) }
                .buttonStyle(.plain)
                .font(Tipo.texto(13, peso: 550))
                .foregroundStyle(Color.huesoSuave)
                .keyboardShortcut("q")
        }
        .padding(18)
        .frame(width: 320)
        .background {
            ZStack {
                Color.noche
                FondoVivo(calor: 0.3, humo: 0.8, quieto: true).padding(-40).offset(y: 60).opacity(0.9)
            }
        }
        .tint(.hueso)
    }

    private func abrir(_ s: Seccion) {
        nav.seccion = s
        openWindow(id: "main")
        NSApp.activate()
    }
}

/// What the menu bar icon says: at rest, recording, speaking, or not connected.
enum IconoModo: Equatable {
    case reposo, desconectado, grabando, hablando

    init(_ state: AgentState, _ c: Conversacion) {
        // shortcut: nothing plays over `/events`, so `hablando` never shows until PR 5's voice.
        if c.grabando { self = .grabando }
        else { self = state.status?.phase == .ready ? .reposo : .desconectado }
    }

    var texto: String? {
        switch self {
        case .grabando: "grabando"
        case .hablando: "hablando"
        case .reposo, .desconectado: nil
        }
    }
}

/// The menu bar icon: the F of the brand as a template image (follows the menu bar's own colour),
/// with a small dot when Fogar is not connected, a solid ring while recording and two sound arcs
/// while speaking. VoiceOver reads the state.
struct MenuBarIcono: View {
    let modo: IconoModo
    let titular: String

    var body: some View {
        Image(nsImage: Self.icono(modo))
            .accessibilityLabel("Fogar: \(modo.texto.map { $0.prefix(1).uppercased() + $0.dropFirst() } ?? titular)")
    }

    private static var cache: [IconoModo: NSImage] = [:]

    static func icono(_ modo: IconoModo) -> NSImage {
        if let img = cache[modo] { return img }
        let geo = LineaGeometry.shared
        let end = geo.idx["bar"]?.1 ?? 0
        let pts = Array(geo.word[0...end])
        let xs = pts.map(\.x), ys = pts.map(\.y)
        let minX = xs.min() ?? 0, maxX = xs.max() ?? 1, minY = ys.min() ?? 0, maxY = ys.max() ?? 1
        // Wider while recording or speaking: the mark sits beside the F, not on it.
        let ancho: CGFloat = modo == .grabando || modo == .hablando ? 26 : 18
        let size = NSSize(width: ancho, height: 18)
        let img = NSImage(size: size, flipped: true) { rect in
            let caja = NSRect(x: 0, y: 0, width: 18, height: rect.height)
            let k = min((caja.width - 3) / (maxX - minX), (caja.height - 3) / (maxY - minY))
            let ox = (caja.width - (maxX - minX) * k) / 2, oy = (caja.height - (maxY - minY) * k) / 2
            let path = NSBezierPath()
            for (i, p) in pts.enumerated() {
                let q = NSPoint(x: ox + (p.x - minX) * k, y: oy + (p.y - minY) * k)
                if i == 0 { path.move(to: q) } else { path.line(to: q) }
            }
            path.lineWidth = 1.6
            path.lineCapStyle = .round
            path.lineJoinStyle = .round
            NSColor.black.setStroke()
            NSColor.black.setFill()
            path.stroke()
            switch modo {
            case .reposo:
                break
            case .desconectado:
                NSBezierPath(ovalIn: NSRect(x: rect.width - 6, y: rect.height - 6, width: 5, height: 5)).fill()
            case .grabando:
                NSBezierPath(ovalIn: NSRect(x: 17, y: 5, width: 8, height: 8)).fill()
            case .hablando:
                for (r, x) in [(3.5, 18.0), (6.5, 18.0)] {
                    let arco = NSBezierPath()
                    arco.appendArc(withCenter: NSPoint(x: x, y: 9), radius: r, startAngle: -50, endAngle: 50)
                    arco.lineWidth = 1.5
                    arco.lineCapStyle = .round
                    arco.stroke()
                }
            }
            return true
        }
        img.isTemplate = true
        cache[modo] = img
        return img
    }
}
