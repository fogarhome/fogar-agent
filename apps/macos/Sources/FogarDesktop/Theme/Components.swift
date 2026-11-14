import SwiftUI

// The dashboard's components (FgBoton, FgEstado, FgCampo, the lienzo nodes) as native SwiftUI.

/// FgBoton: a pill. `principal` is the inverted ink (one per screen); `discreto`, a bone veil.
/// Presses to 97 % in 120 ms; disabled at 42 % opacity.
struct Pildora: ButtonStyle {
    enum Variante { case principal, discreto }
    var variante: Variante = .discreto
    var pequena = false
    var ocupado = false
    /// Fills the available width (the label and spinner stay centred together).
    var ancho = false

    @Environment(\.isEnabled) private var isEnabled
    @State private var hover = false

    func makeBody(configuration: Configuration) -> some View {
        HStack(spacing: 8) {
            if ocupado { Giro().frame(width: 14, height: 14) }
            configuration.label
        }
        .font(Tipo.texto(pequena ? 14 : 15, peso: 550))
        .lineLimit(1)
        .padding(.horizontal, pequena ? 14 : 20)
        .frame(maxWidth: ancho ? .infinity : nil, minHeight: pequena ? 32 : 40)
        .foregroundStyle(variante == .principal ? Color.sobreAccion : Color.hueso)
        .background(Capsule().fill(fondo(configuration)))
        .contentShape(Capsule())
        .scaleEffect(configuration.isPressed && isEnabled ? 0.97 : 1)
        .opacity(isEnabled ? 1 : 0.42)
        .animation(.timingCurve(0.23, 1, 0.32, 1, duration: Medida.pulsar), value: configuration.isPressed)
        .animation(.easeOut(duration: Medida.rapido), value: hover)
        .onHover { hover = $0 }
        .accessibilityAddTraits(ocupado ? .updatesFrequently : [])
    }

    private func fondo(_ c: Configuration) -> Color {
        switch variante {
        case .principal: hover && isEnabled ? .accionAlta : .accion
        case .discreto: hover && isEnabled ? .veloAlto : .velo
        }
    }
}

/// The busy spinner of FgBoton (a ring with a gap). Still under reduce motion.
struct Giro: View {
    @Environment(\.accessibilityReduceMotion) private var reduce
    @State private var vuelta = false
    var body: some View {
        Circle().trim(from: 0, to: 0.75)
            .stroke(style: StrokeStyle(lineWidth: 2, lineCap: .round))
            .rotationEffect(.degrees(vuelta ? 360 : 0))
            .onAppear {
                guard !reduce else { return }
                withAnimation(.linear(duration: 0.8).repeatForever(autoreverses: false)) { vuelta = true }
            }
            .accessibilityHidden(true)
    }
}

/// FgEstado: a state is a flat colour, its own icon shape and its text (colour never alone).
/// ok → square with tick · warn → half-filled rhombus · bad → triangle · off → dashed square.
enum Tono: Equatable { case ok, warn, bad, off }

struct EstadoIcono: View {
    let tono: Tono
    var tam: CGFloat = 14

    var body: some View {
        Canvas { ctx, size in
            let s = size.width / 14
            func p(_ x: CGFloat, _ y: CGFloat) -> CGPoint { CGPoint(x: x * s, y: y * s) }
            switch tono {
            case .ok:
                ctx.fill(Path(roundedRect: CGRect(x: s, y: s, width: 12 * s, height: 12 * s), cornerRadius: 3.5 * s), with: .color(.correcto))
                var tick = Path()
                tick.move(to: p(4, 7.3)); tick.addLine(to: p(6, 9.3)); tick.addLine(to: p(10.1, 4.8))
                ctx.stroke(tick, with: .color(.noche), style: StrokeStyle(lineWidth: 1.8 * s, lineCap: .round, lineJoin: .round))
            case .warn:
                var rombo = Path()
                rombo.move(to: p(7, 1.2)); rombo.addLine(to: p(12.8, 7)); rombo.addLine(to: p(7, 12.8)); rombo.addLine(to: p(1.2, 7)); rombo.closeSubpath()
                ctx.stroke(rombo, with: .color(.degradado), style: StrokeStyle(lineWidth: 1.6 * s, lineJoin: .round))
                var mitad = Path()
                mitad.move(to: p(7, 1.2)); mitad.addLine(to: p(12.8, 7)); mitad.addLine(to: p(7, 12.8)); mitad.closeSubpath()
                ctx.fill(mitad, with: .color(.degradado))
            case .bad:
                var tri = Path()
                tri.move(to: p(7, 1.4)); tri.addLine(to: p(13.1, 12.3)); tri.addLine(to: p(0.9, 12.3)); tri.closeSubpath()
                ctx.fill(tri, with: .color(.error))
                ctx.stroke(tri, with: .color(.error), style: StrokeStyle(lineWidth: 0.8 * s, lineJoin: .round))
                var palo = Path()
                palo.move(to: p(7, 5.1)); palo.addLine(to: p(7, 8.5))
                ctx.stroke(palo, with: .color(.noche), style: StrokeStyle(lineWidth: 1.7 * s, lineCap: .round))
                ctx.fill(Path(ellipseIn: CGRect(x: 6.05 * s, y: 9.35 * s, width: 1.9 * s, height: 1.9 * s)), with: .color(.noche))
            case .off:
                ctx.stroke(Path(roundedRect: CGRect(x: 1.8 * s, y: 1.8 * s, width: 10.4 * s, height: 10.4 * s), cornerRadius: 3 * s),
                           with: .color(.noDisponible), style: StrokeStyle(lineWidth: 1.6 * s, dash: [2.6 * s, 2 * s]))
            }
        }
        .frame(width: tam, height: tam)
        .accessibilityHidden(true)
    }
}

/// FgEstado with its text.
struct Estado: View {
    let tono: Tono
    let texto: String
    var body: some View {
        HStack(spacing: 8) {
            EstadoIcono(tono: tono)
            Text(texto)
        }
        .accessibilityElement(children: .combine)
    }
}

/// The lienzo node: thin glass over the living background, hairline inner border and top reflection.
struct Nodo: ViewModifier {
    var encendido = false
    var punteado = false
    func body(content: Content) -> some View {
        content
            .padding(.horizontal, 14)
            .padding(.vertical, 8)
            .frame(minHeight: 56, alignment: .leading)
            .background {
                RoundedRectangle(cornerRadius: Medida.radioNodo)
                    .fill(punteado ? Color.clear : Color.vidrioFino)
            }
            .overlay {
                RoundedRectangle(cornerRadius: Medida.radioNodo)
                    .strokeBorder(encendido ? Color.llama.opacity(0.42) : Color.vidrioBorde,
                                  style: StrokeStyle(lineWidth: 1, dash: punteado ? [5, 6] : []))
            }
            .overlay(alignment: .top) {
                if !punteado {
                    Rectangle().fill(Color.vidrioReflejo).frame(height: 1)
                        .padding(.horizontal, Medida.radioNodo / 2)
                }
            }
    }
}

extension View {
    func nodo(encendido: Bool = false, punteado: Bool = false) -> some View {
        modifier(Nodo(encendido: encendido, punteado: punteado))
    }

    /// `fg-meta`
    func meta() -> some View { font(Tipo.meta).foregroundStyle(Color.huesoTenue) }
    /// `fg-suave`
    func suave() -> some View { foregroundStyle(Color.huesoSuave) }
}

/// FgCampo: label, field, hint, and the error next to the field (with its triangle).
struct Campo<Field: View>: View {
    let etiqueta: String
    var pista: String?
    var error: String?
    @ViewBuilder let campo: Field
    @FocusState.Binding var enfocado: Bool

    var body: some View {
        VStack(alignment: .leading, spacing: 8) {
            Text(etiqueta).font(Tipo.etiqueta).foregroundStyle(Color.hueso)
            campo
                .textFieldStyle(.plain)
                .font(Tipo.texto(15))
                .foregroundStyle(Color.hueso)
                .padding(.horizontal, 14)
                .frame(height: 44)
                .background(RoundedRectangle(cornerRadius: Medida.radioNodo).fill(Color.noche))
                .overlay {
                    RoundedRectangle(cornerRadius: Medida.radioNodo)
                        .strokeBorder(error != nil ? Color.error : Color.veta, lineWidth: 1)
                }
                // FgCampo: the focus ring is a 2 pt bone outline outside the field, so an error
                // border stays visible while the field is focused.
                .overlay {
                    RoundedRectangle(cornerRadius: Medida.radioNodo + 3)
                        .strokeBorder(Color.hueso, lineWidth: 2)
                        .padding(-3)
                        .opacity(enfocado ? 1 : 0)
                }
                .focused($enfocado)
                .accessibilityLabel(etiqueta)
                .accessibilityHint(error ?? pista ?? "")
            if let error {
                HStack(alignment: .firstTextBaseline, spacing: 7) {
                    EstadoIcono(tono: .bad, tam: 12)
                    Text(error).foregroundStyle(Color.hueso)
                }
                .font(Tipo.meta)
                .accessibilityElement(children: .combine)
            } else if let pista {
                Text(pista).meta()
            }
        }
    }
}
