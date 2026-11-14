import AppKit
import FogarWire
import SwiftUI

/// The menu bar quick view: the state at a glance and a way into the window. Never animates.
struct MenuBarView: View {
    @Environment(AgentLink.self) private var link
    @Environment(Navegacion.self) private var nav
    @Environment(\.openWindow) private var openWindow

    var body: some View {
        let t = Textos.titular(link.state)
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
                    Text("\(g.address.replacingOccurrences(of: "http://", with: "")) · versión \(g.version)").meta()
                }
            }

            VStack(alignment: .leading, spacing: 8) {
                Button { abrir(.estado) } label: { Text("Abrir Fogar Desktop").frame(maxWidth: .infinity) }
                    .buttonStyle(Pildora(variante: .principal, pequena: true))
                switch link.state {
                case .live(let s) where s.phase == .notPaired:
                    Button { abrir(.emparejar) } label: { Text("Emparejar este Mac…").frame(maxWidth: .infinity) }
                        .buttonStyle(Pildora(pequena: true))
                case .notRunning, .refused:
                    Button { link.retryNow() } label: { Text("Volver a llamar al agente").frame(maxWidth: .infinity) }
                        .buttonStyle(Pildora(pequena: true))
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

/// The menu bar icon: the F of the brand as a template image (follows the menu bar's own colour),
/// with a small mark when Fogar is not connected. VoiceOver reads the state.
struct MenuBarIcono: View {
    let state: AgentState

    var body: some View {
        let ok = state.status?.phase == .ready
        Image(nsImage: Self.icono(marca: !ok))
            .accessibilityLabel("Fogar: \(Textos.titular(state).titulo)")
    }

    private static var cache: [Bool: NSImage] = [:]

    static func icono(marca: Bool) -> NSImage {
        if let img = cache[marca] { return img }
        let geo = LineaGeometry.shared
        let end = geo.idx["bar"]?.1 ?? 0
        let pts = Array(geo.word[0...end])
        let xs = pts.map(\.x), ys = pts.map(\.y)
        let minX = xs.min() ?? 0, maxX = xs.max() ?? 1, minY = ys.min() ?? 0, maxY = ys.max() ?? 1
        let size = NSSize(width: 18, height: 18)
        let img = NSImage(size: size, flipped: true) { rect in
            let k = min((rect.width - 3) / (maxX - minX), (rect.height - 3) / (maxY - minY))
            let ox = (rect.width - (maxX - minX) * k) / 2, oy = (rect.height - (maxY - minY) * k) / 2
            let path = NSBezierPath()
            for (i, p) in pts.enumerated() {
                let q = NSPoint(x: ox + (p.x - minX) * k, y: oy + (p.y - minY) * k)
                if i == 0 { path.move(to: q) } else { path.line(to: q) }
            }
            path.lineWidth = 1.6
            path.lineCapStyle = .round
            path.lineJoinStyle = .round
            NSColor.black.setStroke()
            path.stroke()
            if marca {
                NSColor.black.setFill()
                NSBezierPath(ovalIn: NSRect(x: rect.width - 6, y: rect.height - 6, width: 5, height: 5)).fill()
            }
            return true
        }
        img.isTemplate = true
        cache[marca] = img
        return img
    }
}
