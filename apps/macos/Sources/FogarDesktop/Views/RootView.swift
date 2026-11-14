import FogarWire
import SwiftUI

/// The main window: the dashboard's sidebar (BarraLateral) and the selected section on the night.
struct RootView: View {
    @Environment(AgentLink.self) private var link
    @Environment(Navegacion.self) private var nav

    var body: some View {
        @Bindable var nav = nav
        NavigationSplitView {
            BarraLateral(seleccion: $nav.seccion)
                .navigationSplitViewColumnWidth(min: 200, ideal: 232, max: 280)
        } detail: {
            ZStack {
                Color.noche.ignoresSafeArea()
                switch nav.seccion {
                case .estado: EstadoView()
                case .emparejar: EmparejarView()
                case .ajustes: AjustesView(enVentana: true)
                }
            }
            .modifier(BarraSinFondo())
        }
        .frame(minWidth: 940, minHeight: 620)
        .background(Color.noche)
        .tint(.hueso)
        .vigilaVisibilidad()
        .onChange(of: link.state.status?.phase) { old, new in
            // Revoked or unpaired elsewhere: go where the person can fix it.
            if new == .notPaired, let old, old != .notPaired { nav.seccion = .emparejar }
        }
        // Announce a new headline, not every counter tick.
        .onChange(of: Textos.titular(link.state).titulo) { _, titulo in
            AccessibilityNotification.Announcement(titulo).post()
        }
    }

}

/// BarraLateral: the word, the sections, and at the foot what the app knows of the agent.
struct BarraLateral: View {
    @Binding var seleccion: Seccion
    @Environment(AgentLink.self) private var link

    var body: some View {
        // Drawn like BarraLateral.vue: the selected section sits on piedra alta. A native List would
        // paint the selection in the person's system accent colour, which is not Fogar's.
        VStack(spacing: 2) {
            ForEach(Seccion.allCases) { s in
                FilaSeccion(seccion: s, activa: s == seleccion) { seleccion = s }
            }
            Spacer(minLength: 0)
        }
        .padding(.horizontal, 12)
        .padding(.top, 8)
        .frame(maxHeight: .infinity, alignment: .top)
        .accessibilityElement(children: .contain)
        .accessibilityLabel("Secciones")
        .safeAreaInset(edge: .top, spacing: 18) {
            HStack(alignment: .center, spacing: 8) {
                LineaViva(pose: .rest, etiqueta: "Fogar", quieta: true)
                    .frame(width: 74)
                Text("Desktop").meta()
            }
            .frame(maxWidth: .infinity, alignment: .leading)
            .padding(.horizontal, 18)
            .padding(.top, 4)
            .accessibilityElement(children: .combine)
            .accessibilityLabel("Fogar Desktop")
        }
        .safeAreaInset(edge: .bottom) {
            Pie(state: link.state)
                .padding(.horizontal, 18)
                .padding(.bottom, 16)
        }
    }
}

private struct FilaSeccion: View {
    let seccion: Seccion
    let activa: Bool
    let elegir: () -> Void
    @State private var hover = false

    var body: some View {
        Button(action: elegir) {
            HStack(spacing: 12) {
                Image(systemName: seccion.icono)
                    .font(.system(size: 14, weight: .regular))
                    .frame(width: 18)
                Text(seccion.titulo).font(Tipo.texto(15, peso: 550))
                Spacer(minLength: 0)
            }
            .foregroundStyle(activa || hover ? Color.hueso : Color.huesoSuave)
            .padding(.horizontal, 10)
            .frame(height: 36)
            .background {
                RoundedRectangle(cornerRadius: 10)
                    .fill(activa ? Color.piedraAlta : hover ? Color.velo.opacity(0.5) : .clear)
            }
            .contentShape(RoundedRectangle(cornerRadius: 10))
        }
        .buttonStyle(.plain)
        .onHover { hover = $0 }
        .animation(.easeOut(duration: Medida.rapido), value: hover)
        .accessibilityAddTraits(activa ? .isSelected : [])
        .help("\(seccion.titulo) (⌘\(String(seccion.atajo.character)))")
    }
}

private struct Pie: View {
    let state: AgentState
    var body: some View {
        VStack(alignment: .leading, spacing: 4) {
            Estado(tono: tono, texto: texto)
                .font(Tipo.texto(12, peso: 550))
                .foregroundStyle(Color.huesoSuave)
        }
        .frame(maxWidth: .infinity, alignment: .leading)
    }

    private var tono: Tono {
        switch state {
        case .live: .ok
        case .looking: .off
        default: .bad
        }
    }

    private var texto: String {
        switch state {
        case .live: "Agente en marcha"
        case .looking: "Buscando al agente…"
        case .notRunning: "Agente parado"
        case .versionMismatch: "Agente de otra versión"
        case .refused: "Agente rechaza la app"
        case .noLiveStatus, .unintelligible: "Agente desactualizado"
        }
    }
}

/// The title bar keeps no band of its own: the night (and the living background) runs under it,
/// like the dashboard, which has no top bar at all.
private struct BarraSinFondo: ViewModifier {
    func body(content: Content) -> some View {
        if #available(macOS 15, *) {
            content.toolbarBackgroundVisibility(.hidden, for: .windowToolbar)
        } else {
            content.toolbarBackground(.hidden, for: .windowToolbar)
        }
    }
}
